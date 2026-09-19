use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, VId};

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
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn output(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

#[test]
fn ordered_steps_see_the_overlay_and_publish_one_commit() {
    let ((), report) = run_async_under_lab(0x7478_6301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "CREATE (n:Person {p: 4})",
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
            "--write",
            "MATCH (n:Person) SET n.p = 7",
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &options, None, true, &mut bytes).await);
        let text = output(bytes);
        assert!(
            text.contains(r#""statement":2,"cells":[{"type":"int","value":"4"}]"#),
            "{text}"
        );
        assert!(
            text.contains(r#""statement":4,"cells":[{"type":"int","value":"7"}]"#),
            "{text}"
        );
        assert!(text.ends_with(
            "\"kind\":\"committed\",\"basis\":0,\"seq\":1,\"count\":2,\"statements\":4}\n"
        ));
        assert_eq!(text.matches("\"event\":\"result\"").count(), 1);
        assert_eq!(text.matches("\"view\":\"transaction_local\"").count(), 4);
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        let rows = db.vertices_at(CommitSeq(1)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].props,
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rollback_discards_effects_and_rows_but_read_only_finish_creates_no_marker() {
    let ((), report) = run_async_under_lab(0x7478_6302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let rollback = options(&[
            "--write",
            "CREATE (n:Person {p: 4})",
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
            "--rollback",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &rollback, None, true, &mut bytes).await);
        let text = output(bytes);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"kind\":\"rolled_back\""));
        assert!(!text.contains("\"event\":\"row\""));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        assert_eq!(contexts.txn().outstanding_obligations(), 0);

        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(500),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(9))],
        );
        let basis = db.write(&contexts.commit(), batch).await.unwrap();
        let reads = options(&[
            "--query",
            "MATCH (n) RETURN n.p AS p",
            "--query",
            "MATCH (n) RETURN COUNT(*) AS total",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &reads, None, true, &mut bytes).await);
        assert!(output(bytes).contains("\"kind\":\"read_closed\""));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.delta_since(basis).unwrap().count(), 0);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_row_and_encoded_output_refusals_abort_the_entire_workspace() {
    let ((), report) = run_async_under_lab(0x7478_6303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for limits in [
            Limits {
                rows: 1,
                output_bytes: 100_000,
            },
            Limits {
                rows: 100,
                output_bytes: 1,
            },
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let options = options(&[
                "--write",
                "CREATE (n:Person {p: 4})",
                "--query",
                "MATCH (n:Person) RETURN n.p AS p",
                "--query",
                "MATCH (n:Person) RETURN n.p AS p",
            ]);
            let mut bytes = Vec::new();
            let error = run_with_limits(
                &mut db, &contexts, &options, None, true, &mut bytes, limits, None,
            )
            .await
            .err()
            .expect("bounded output must refuse");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(
                bytes.is_empty(),
                "a successful prefix escaped an aborted transaction"
            );
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        // An exhausted row allowance still permits a genuinely empty later read.
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "CREATE (n:Person {p: 4})",
            "--query",
            "MATCH (n) RETURN n.p AS p",
            "--query",
            "MATCH (n) WHERE n.p > 100 RETURN n.p AS p",
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
        assert!(output(bytes).contains("\"kind\":\"committed\""));
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn each_step_binds_its_own_parameters_without_text_interpolation() {
    let ((), report) = run_async_under_lab(0x7478_6304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "CREATE (n:Person {p: $value})",
            "--param",
            "value=int:4",
            "--query",
            "MATCH (n) WHERE n.p > $floor RETURN n.p AS p",
            "--param",
            "floor=int:3",
            "--write",
            "MATCH (n) SET n.p = $value",
            "--param",
            "value=text:semi; 'quote'\nnext",
            "--query",
            "MATCH (n) RETURN n.p AS p",
        ]);
        assert!(options.raw_params.is_empty());
        assert_eq!(options.steps[0].raw_params[0].1, "int:4");
        assert_eq!(options.steps[2].raw_params[0].1, "text:semi; 'quote'\nnext");
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &options, None, true, &mut bytes).await);
        let text = output(bytes);
        assert!(text.contains(r#""type":"int","value":"4""#));
        assert!(text.contains(r#"semi; 'quote'\nnext"#));
        let rows = db.vertices_at(db.frontier().unwrap()).unwrap();
        assert_eq!(
            rows[0].props[0].1,
            CanonicalScalar::ucs_basic_text("semi; 'quote'\nnext").unwrap()
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_tail_and_temporal_reads_cannot_publish_a_valid_write_prefix() {
    let ((), report) = run_async_under_lab(0x7478_6305, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for tail in [
            "not a query",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n",
            "MATCH (n) WHERE n.p > $missing RETURN n",
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let options = options(&["--write", "CREATE (n:Person {p: 4})", "--query", tail]);
            let mut bytes = Vec::new();
            let error = run(&mut db, &contexts, &options, None, true, &mut bytes)
                .await
                .err()
                .unwrap();
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
fn output_failure_does_not_undo_or_misreport_a_durable_commit() {
    let ((), report) = run_async_under_lab(0x7478_6306, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&["--write", "CREATE (n:Person {p: 4})"]);
        let error = run(&mut db, &contexts, &options, None, true, &mut BrokenOutput)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, 5);
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
fn ambiguous_native_completion_never_emits_rows_or_a_rollback_claim() {
    let ((), report) = run_async_under_lab(0x7478_6307, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "CREATE (n:Person {p: 4})",
            "--query",
            "MATCH (n) RETURN n",
        ]);
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
        .err()
        .expect("injected native completion failure");
        assert_eq!(error.code, 5, "{}", error.message);
        assert!(bytes.is_empty());
        assert!(!error.message.contains("rolled_back"));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parser_refuses_unscoped_parameters_empty_steps_and_inapplicable_options() {
    for tail in [
        vec!["--param", "p=int:4", "--query", "MATCH (n) RETURN n"],
        vec![],
        vec!["--query", ""],
        vec!["--query", "MATCH (n) RETURN n", "--certify-to", "x"],
        vec!["--write", "CREATE (n)", "--rollback", "--rollback"],
    ] {
        let mut args = vec!["--db", "x", "--key-file", "y"];
        args.extend(tail);
        assert!(
            crate::parse(
                &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                "transaction"
            )
            .is_err()
        );
    }
    let mut args = vec![
        "--db".to_owned(),
        "x".to_owned(),
        "--key-file".to_owned(),
        "y".to_owned(),
    ];
    for _ in 0..=MAX_STATEMENTS {
        args.extend(["--query".to_owned(), "MATCH (n) RETURN n".to_owned()]);
    }
    assert!(crate::parse(&args, "transaction").is_err());
    assert!(validate_input(&[Step::new(false, "x".repeat(MAX_INPUT_BYTES + 1))]).is_err());
}
