//! Returning writes use the production transaction driver and native lab/VFS.
//! No graph interpreter, commit mock, or separately committed write is used.

use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::DatabaseKeys;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId};

fn okay<T>(result: Result<T, Failure>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}
fn options(parts: &[&str]) -> Options {
    let mut args = vec![
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--label",
        "Person=1",
        "--label",
        "Copy=2",
        "--relation",
        "R=1",
        "--property",
        "p=1",
    ];
    args.extend_from_slice(parts);
    okay(crate::parse(
        &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        "transaction",
    ))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn rows_at(text: &str, index: usize) -> Vec<&str> {
    let prefix = format!(r#""event":"row","statement":{index},"#);
    text.lines().filter(|line| line.contains(&prefix)).collect()
}

#[test]
fn returning_writes_and_reads_share_one_workspace_and_one_completion() {
    let ((), report) = run_async_under_lab(0x7478_6401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "UNWIND [3,1,3] AS x CREATE (n:Person {p:x}) RETURN n,n.p AS p",
            "--write",
            "MATCH (n:Person) WHERE n.p=1 CREATE (n)-[e:R]->(m:Copy {p:n.p+10}) \
             RETURN n AS source,e,m,m.p AS p",
            "--write",
            "MATCH (n:Person) SET n.p=9",
            "--query",
            "MATCH (n) RETURN n,n.p AS p ORDER BY n",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &options, None, true, &mut bytes).await);
        let text = String::from_utf8(bytes).unwrap();
        let first = rows_at(&text, 1);
        assert_eq!(first.len(), 3, "{text}");
        for (row, (id, value)) in first.iter().zip([(1, 3), (2, 1), (3, 3)]) {
            assert!(row.contains(&format!(
                r#""cells":[{{"type":"vertex","value":"{id}"}},{{"type":"int","value":"{value}"}}]"#
            )), "{row}");
        }
        let second = rows_at(&text, 2);
        assert_eq!(second.len(), 1, "{text}");
        assert!(second[0].contains(
            r#""cells":[{"type":"vertex","value":"2"},{"type":"edge","value":"1"},{"type":"vertex","value":"4"},{"type":"int","value":"11"}]"#
        ), "{text}");
        assert_eq!(rows_at(&text, 4).len(), 4);
        assert!(rows_at(&text, 4)[0].contains(r#""type":"int","value":"9""#));
        assert!(text.contains(
            r#""index":1,"kind":"write","view":"transaction_local","basis":0,"count":3,"statements":1"#
        ), "{text}");
        assert!(
            text.ends_with(
                "\"kind\":\"committed\",\"basis\":0,\"seq\":1,\"count\":8,\"statements\":4}\n"
            ),
            "{text}"
        );
        assert_eq!(text.matches("\"event\":\"result\"").count(), 1);
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 4);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_arguments_remain_local_and_earlier_rows_are_frozen() {
    let ((), report) = run_async_under_lab(0x7478_6402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for robot in [true, false] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let options = options(&[
                "--write",
                "CREATE (n:Person {p:$value}) RETURN n.p AS p",
                "--param",
                "value=int:4",
                "--write",
                "CREATE (n:Person {p:$value}) RETURN n.p AS p",
                "--param",
                "value=text:semi; 'quote'\n雪",
                "--write",
                "MATCH (n:Person) SET n.p=99",
                "--query",
                "MATCH (n:Person) RETURN n.p AS p",
            ]);
            let mut bytes = Vec::new();
            okay(run(&mut db, &contexts, &options, None, robot, &mut bytes).await);
            let text = String::from_utf8(bytes).unwrap();
            if robot {
                assert!(rows_at(&text, 1)[0].contains(r#""type":"int","value":"4""#));
                assert!(rows_at(&text, 2)[0].contains(r#"semi; 'quote'\n雪"#));
                assert_eq!(rows_at(&text, 4).len(), 2);
                assert!(rows_at(&text, 4).iter().all(|row| row.contains("99")));
            } else {
                assert!(text.contains("statement 1: write (transaction-local basis 0)"));
                assert!(text.contains("statement 2: write (transaction-local basis 0)"));
                assert!(text.contains("\\n"));
            }
            let vertices = db.vertices_at(CommitSeq(1)).unwrap();
            assert!(
                vertices
                    .iter()
                    .all(|v| v.props == vec![(PropertyKeyId(1), CanonicalScalar::Int(99))])
            );
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_and_read_rows_consume_one_transaction_wide_allowance() {
    let ((), report) = run_async_under_lab(0x7478_6403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (kind, tail) in [
            ("--query", "MATCH (n:Person) RETURN n"),
            ("--write", "CREATE (n:Person) RETURN n"),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let options = options(&[
                "--write",
                "CREATE (n:Person {p:1}) RETURN n",
                "--query",
                "MATCH (n:Person) RETURN n",
                kind,
                tail,
            ]);
            let mut bytes = Vec::new();
            let error = run_with_limits(
                &mut db,
                &contexts,
                &options,
                None,
                true,
                &mut bytes,
                Limits {
                    rows: 2,
                    output_bytes: 100_000,
                },
                None,
            )
            .await
            .expect_err("the third row must exhaust the shared allowance");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(error.message.contains("step 3"), "{}", error.message);
            assert!(bytes.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "UNWIND [1,1,1] AS x CREATE (n:Person {p:x}) RETURN DISTINCT n.p AS p",
            "--write",
            "UNWIND [2,3] AS x CREATE (n:Person {p:x}) RETURN n LIMIT 0",
            "--query",
            "MATCH (n) WHERE n.p<0 RETURN n",
        ]);
        let mut bytes = Vec::new();
        okay(
            run_with_limits(
                &mut db,
                &contexts,
                &options,
                None,
                true,
                &mut bytes,
                Limits {
                    rows: 1,
                    output_bytes: 100_000,
                },
                None,
            )
            .await,
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(rows_at(&text, 1).len(), 1);
        assert!(rows_at(&text, 2).is_empty());
        assert!(rows_at(&text, 3).is_empty());
        assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 5);
        assert!(text.contains(r#""count":1,"statements":3"#));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_return_errors_and_output_refusals_discard_all_staged_effects() {
    let ((), report) = run_async_under_lab(0x7478_6404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for suffix in ["", " LIMIT 0"] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let tail = format!(
                "UNWIND [4,2,0] AS x CREATE (n:Person {{p:x}}) RETURN 100/x AS ratio{suffix}"
            );
            let options = options(&[
                "--write",
                "CREATE (n:Person {p:5}) RETURN n",
                "--write",
                &tail,
            ]);
            let mut bytes = Vec::new();
            let error = run(&mut db, &contexts, &options, None, true, &mut bytes)
                .await
                .expect_err("late result expression must refuse");
            assert!(error.message.contains("step 2"), "{}", error.message);
            assert!(bytes.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let payload = format!("value=text:{}", "x".repeat(4096));
        let options = options(&[
            "--write",
            "CREATE (n:Person)",
            "--write",
            "CREATE (n:Person {p:$value}) RETURN n.p AS p",
            "--param",
            &payload,
        ]);
        let mut bytes = Vec::new();
        let error = run_with_limits(
            &mut db,
            &contexts,
            &options,
            None,
            true,
            &mut bytes,
            Limits {
                rows: 100,
                output_bytes: 1024,
            },
            None,
        )
        .await
        .expect_err("transport admission must happen before commit");
        assert!(error.message.contains("step 2"), "{}", error.message);
        assert!(
            error.message.contains("encoded output limit"),
            "{}",
            error.message
        );
        assert!(bytes.is_empty());
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_rollback_and_empty_returning_input_publish_no_write_marker() {
    let ((), report) = run_async_under_lab(0x7478_6405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let rollback = options(&[
            "--write",
            "CREATE (n:Person {p:1}) RETURN n",
            "--write",
            "MATCH (n:Person) CREATE (m:Copy {p:n.p}) RETURN n,m",
            "--query",
            "MATCH (n) RETURN n",
            "--rollback",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &rollback, None, true, &mut bytes).await);
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(r#""kind":"rolled_back""#));
        assert!(text.contains(r#""count":0,"statements":3"#));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        let empty = options(&[
            "--write",
            "MATCH (n:Person) CREATE (m:Copy) RETURN n,m",
            "--write",
            "UNWIND [] AS x CREATE (n:Person) RETURN n",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &empty, None, true, &mut bytes).await);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(r#""kind":"read_closed""#));
        assert_eq!(text.matches("\"event\":\"columns\"").count(), 2);
        assert!(!text.contains("\"event\":\"row\""));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_returning_tail_and_missing_parameters_refuse_before_transaction_begin() {
    let ((), report) = run_async_under_lab(0x7478_6406, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (kind, tail) in [
            ("--write", "CREATE (n:Person {p:$value}) RETURN n"),
            ("--write", "CREATE (n:Person) RETURN unknown"),
            ("--write", "CREATE (n:Person) RETURN n; CREATE (m)"),
            ("--query", "CREATE (n:Person) RETURN n"),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let options = options(&[
                "--write",
                "CREATE (n:Person {p:$value}) RETURN n",
                "--param",
                "value=int:4",
                kind,
                tail,
            ]);
            let mut bytes = Vec::new();
            let error = run(&mut db, &contexts, &options, None, true, &mut bytes)
                .await
                .expect_err("a prepared step cannot borrow another step's parameters");
            assert!(error.message.contains("step 2"), "{}", error.message);
            assert!(bytes.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct BrokenOutput;
impl Write for BrokenOutput {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed receiver",
        ))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn returning_completion_distinguishes_ambiguous_commit_from_failed_delivery() {
    let ((), report) = run_async_under_lab(0x7478_6407, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let options = options(&["--write", "CREATE (n:Person {p:4}) RETURN n"]);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        let error = run_with_limits(
            &mut db,
            &contexts,
            &options,
            None,
            true,
            &mut bytes,
            Limits::default(),
            Some(fgdb::CrashPoint::AfterMarkerFileSyncBeforeDirectorySync),
        )
        .await
        .expect_err("injected native completion failure");
        assert_eq!(error.code, 5, "{}", error.message);
        assert!(bytes.is_empty());
        assert!(!error.message.contains("rolled_back"));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);

        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let error = run(&mut db, &contexts, &options, None, true, &mut BrokenOutput)
            .await
            .expect_err("completed write cannot reach a closed receiver");
        assert_eq!(error.code, 5, "{}", error.message);
        assert!(
            error.message.contains("committed at seq 1"),
            "{}",
            error.message
        );
        assert!(error.message.contains("do not blindly retry"));
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_steps_count_toward_the_same_native_statement_limit_as_scripts() {
    let ((), report) = run_async_under_lab(0x7478_6408, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for count in [MAX_STATEMENTS - 1, MAX_STATEMENTS] {
            let script = vec!["CREATE (n)"; count].join("; ");
            let options = options(&["--write", &script, "--write", "CREATE (n) RETURN n"]);
            let result = prepare(&options, None, &contexts.query());
            if count < MAX_STATEMENTS {
                let (steps, statements) = okay(result);
                assert_eq!(statements, MAX_STATEMENTS);
                assert!(matches!(&steps[0], PreparedStep::Write(_)));
                assert!(matches!(&steps[1], PreparedStep::Returning(_)));
            } else {
                let error = result.err().expect("the returning step is statement 65");
                assert!(
                    error.message.contains("64 native statements"),
                    "{}",
                    error.message
                );
            }
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
