use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, VId,
};

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
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7).into())]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hybrid_reads_compose_with_staged_writes_and_commit_only_after_output_admission() {
    let ((), report) = run_async_under_lab(0x7478_6320, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let options = options(&[
            "--property",
            "bio=2",
            "--write",
            "CREATE (a:Person {p: 0, bio: 'graph memory'}), \
                    (b:Person {p: 10, bio: 'unrelated'})",
            "--query",
            "CALL hybrid.search(text => 'graph', text_property => 'bio', \
               vector => [0], vector_properties => ['p'], label => 'Person', \
               k => 1, candidates => 2) YIELD node \
             RETURN node.bio AS bio",
            "--write",
            "MATCH (n:Person) SET n.bio = 'graph memory updated'",
            "--query",
            "CALL hybrid.search(text => 'graph', text_property => 'bio', \
               vector => [0], vector_properties => ['p'], label => 'Person', \
               k => 10) YIELD node RETURN count(*) AS total",
        ]);
        for max_rows in [2, 1] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut bytes = Vec::new();
            let result = run_with_limits(
                &mut db,
                &contexts,
                &options,
                None,
                true,
                &mut bytes,
                Limits {
                    rows: max_rows,
                    output_bytes: 100_000,
                },
                None,
            )
            .await;
            if max_rows == 1 {
                let error = result.expect_err("the final aggregate row exceeds delivery capacity");
                assert_eq!(error.code, 3, "{}", error.message);
                assert!(
                    bytes.is_empty(),
                    "failed search exposed a transaction prefix"
                );
                assert_eq!(db.frontier().unwrap(), CommitSeq(0));
                assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            } else {
                okay(result);
                let text = output(bytes);
                assert!(
                    text.contains(
                        r#""statement":2,"cells":[{"type":"text","value":"graph memory"}]"#
                    ),
                    "{text}"
                );
                assert!(
                    text.contains(r#""statement":4,"cells":[{"type":"count","value":"2"}]"#),
                    "{text}"
                );
                assert!(text.ends_with(
                    "\"kind\":\"committed\",\"basis\":0,\"seq\":1,\"count\":2,\"statements\":4}\n"
                ));
                assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
                let vertices = db.vertices_at(CommitSeq(1)).unwrap();
                assert_eq!(vertices.len(), 2);
                let updated = CanonicalScalar::ucs_basic_text("graph memory updated").unwrap();
                assert!(vertices.iter().all(|vertex| {
                    vertex
                        .props
                        .contains(&(PropertyKeyId(2), updated.clone().into()))
                }));
            }
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_hybrid_transaction_rows_follow_savepoints_and_atomic_delivery() {
    let ((), report) = run_async_under_lab(0x7478_6321, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = "CALL hybrid.search(text => 'graph', text_property => 'bio', \
            vector => [0], vector_properties => ['p'], label => 'Person', \
            seeds => [1], relation => 'R', max_hops => 2, \
            eligible => $allowed, candidates => 1, graph_candidates => 1, k => 1) \
            YIELD node, graph_hops, vector_rank, text_rank \
            RETURN node.p AS p, graph_hops, vector_rank, text_rank";
        let options = options(&[
            "--property",
            "bio=2",
            "--relation",
            "R=1",
            "--write",
            "MATCH (n:Person {p:3}) SET n.p = 4",
            "--query",
            query,
            "--param",
            "allowed=json:[3,3]",
            "--savepoint",
            "selected",
            "--write",
            "MATCH (n:Person {p:4}) SET n.p = 9",
            "--query",
            query,
            "--param",
            "allowed=json:[3]",
            "--rollback-to",
            "selected",
            "--query",
            query,
            "--param",
            "allowed=json:[3]",
        ]);
        for max_rows in [2, 1] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=3 {
                seed.create_vertex(
                    VId(id),
                    vec![LabelId(1)],
                    vec![
                        (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                        (
                            PropertyKeyId(2),
                            CanonicalScalar::ucs_basic_text("graph").unwrap(),
                        ),
                    ],
                );
            }
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            seed.add_edge(EId(2), VId(2), VId(3), vec![]);
            db.write(&contexts.commit(), seed).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut bytes = Vec::new();
            let result = run_with_limits(
                &mut db,
                &contexts,
                &options,
                None,
                true,
                &mut bytes,
                Limits {
                    rows: max_rows,
                    output_bytes: 100_000,
                },
                None,
            )
            .await;
            if max_rows == 1 {
                let error =
                    result.expect_err("filtered rows still spend the shared delivery allowance");
                assert_eq!(error.code, 3, "{}", error.message);
                assert!(
                    bytes.is_empty(),
                    "a refused transaction exposed buffered search rows"
                );
                assert_eq!(db.frontier().unwrap(), basis);
            } else {
                okay(result);
                let text = output(bytes);
                let cells = r#""cells":[{"type":"int","value":"4"},{"type":"int","value":"2"},{"type":"int","value":"1"},{"type":"int","value":"1"}]"#;
                assert_eq!(text.matches(cells).count(), 2, "{text}");
                assert!(
                    text.contains(&format!(r#""statement":2,{cells}"#)),
                    "{text}"
                );
                assert!(
                    text.contains(&format!(r#""statement":7,{cells}"#)),
                    "{text}"
                );
                assert!(!text.contains(r#""event":"row","statement":5"#), "{text}");
                assert_eq!(text.matches("\"event\":\"result\"").count(), 1);
                assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
            }
            let vertices = db.vertices_at(db.frontier().unwrap()).unwrap();
            let selected = vertices.iter().find(|row| row.vid == VId(3)).unwrap();
            let expected = if max_rows == 1 { 3 } else { 4 };
            assert!(
                selected
                    .props
                    .contains(&(PropertyKeyId(1), CanonicalScalar::Int(expected).into()))
            );
            assert_eq!(contexts.outstanding_obligations(), 0);
        }
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
            .expect_err("bounded output must refuse");
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
            CanonicalScalar::ucs_basic_text("semi; 'quote'\nnext")
                .unwrap()
                .into()
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
        .expect_err("injected native completion failure");
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
    assert!(
        validate_input(&[Step::new(StepKind::Query, "x".repeat(MAX_INPUT_BYTES + 1))]).is_err()
    );
}

fn committed_values(db: &Database<impl Vfs + Clone>) -> Vec<i64> {
    let mut values: Vec<_> = db
        .vertices_at(db.frontier().unwrap())
        .unwrap()
        .into_iter()
        .map(|row| match &row.props[0].1 {
            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)) => *value,
            other => panic!("unexpected property {other:?}"),
        })
        .collect();
    values.sort_unstable();
    values
}

#[test]
fn rollback_to_a_savepoint_rewinds_later_effects_and_the_rows_they_buffered() {
    let ((), report) = run_async_under_lab(0x7478_6308, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let options = options(&[
            "--write",
            "CREATE (n:Person {p: 1})",
            "--savepoint",
            "s",
            "--write",
            "CREATE (n:Person {p: 2})",
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
            "--rollback-to",
            "s",
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
            "--write",
            "CREATE (n:Person {p: 3})",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &options, None, true, &mut bytes).await);
        let text = output(bytes);
        assert!(
            text.contains(
                r#""index":2,"kind":"savepoint","view":"transaction_local","basis":0,"name":"s"}"#
            ),
            "{text}"
        );
        assert!(text.contains(r#""index":5,"kind":"rollback_to""#), "{text}");
        // Steps 3 and 4 previewed discarded effects: their records and rows go.
        assert!(!text.contains(r#""index":3,"#), "{text}");
        assert!(!text.contains(r#""statement":4"#), "{text}");
        assert!(
            text.contains(r#""statement":6,"cells":[{"type":"int","value":"1"}]"#),
            "{text}"
        );
        assert_eq!(
            text.matches(r#""statement":6,"cells""#).count(),
            1,
            "{text}"
        );
        assert!(
            text.ends_with(
                "\"kind\":\"committed\",\"basis\":0,\"seq\":1,\"count\":1,\"statements\":5}\n"
            ),
            "{text}"
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(committed_values(&db), vec![1, 3]);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn release_keeps_effects_and_a_reused_name_shadows_until_released() {
    let ((), report) = run_async_under_lab(0x7478_6309, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let release = options(&[
            "--write",
            "CREATE (n:Person {p: 1})",
            "--savepoint",
            "a",
            "--write",
            "CREATE (n:Person {p: 2})",
            "--release",
            "a",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &release, None, false, &mut bytes).await);
        let text = output(bytes);
        assert!(text.contains("statement 2: savepoint a\n"), "{text}");
        assert!(text.contains("statement 4: release a\n"), "{text}");
        assert_eq!(committed_values(&db), vec![1, 2]);

        // The younger "a" shadows the older one: the first rollback discards
        // only p 6; once released, the next rollback reaches the older "a"
        // and discards p 5 as well.
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let shadowed = options(&[
            "--savepoint",
            "a",
            "--write",
            "CREATE (n:Person {p: 5})",
            "--savepoint",
            "a",
            "--write",
            "CREATE (n:Person {p: 6})",
            "--rollback-to",
            "a",
            "--release",
            "a",
            "--rollback-to",
            "a",
            "--write",
            "CREATE (n:Person {p: 7})",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &shadowed, None, true, &mut bytes).await);
        assert_eq!(committed_values(&db), vec![7]);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn savepoint_steps_refuse_unknown_names_parameters_and_empty_names_before_running() {
    for tail in [
        vec!["--write", "CREATE (n)", "--rollback-to", "a"],
        vec!["--write", "CREATE (n)", "--release", "a"],
        vec!["--savepoint", "a", "--release", "a", "--rollback-to", "a"],
        vec!["--savepoint", "a", "--rollback-to", "A"],
        vec!["--savepoint", "a", "--param", "p=int:4"],
        vec!["--savepoint", ""],
    ] {
        let mut args = vec!["--db", "x", "--key-file", "y"];
        args.extend(tail.iter().copied());
        let refused = crate::parse(
            &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            "transaction",
        );
        assert!(
            matches!(&refused, Err(error) if error.code == 2),
            "{tail:?} must be a usage error"
        );
    }
    // Rolling back to a savepoint keeps it live: it can be the target again.
    let again = options(&[
        "--savepoint",
        "a",
        "--rollback-to",
        "a",
        "--rollback-to",
        "a",
    ]);
    assert!(validate_input(&again.steps).is_ok());
}

/// A statement given to the wrong step flag names the flag it needs, as a
/// usage error, before the transaction begins (fgdb-j72ep). Text that is
/// neither a read nor a write keeps its own grammar error.
#[test]
fn a_statement_under_the_wrong_step_flag_names_the_right_one() {
    let ((), report) = run_async_under_lab(0x7478_6340, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (parts, expected, code) in [
            (
                ["--query", "CREATE (n:Person {p: 1})"],
                "a write statement: pass it with --write, not --query",
                2,
            ),
            (
                ["--query", "MATCH (n:Person) SET n.p = 2 RETURN n.p AS p"],
                "a write statement: pass it with --write, not --query",
                2,
            ),
            (
                ["--write", "MATCH (n:Person) RETURN count(*) AS c"],
                "a read statement: pass it with --query, not --write",
                2,
            ),
            (["--write", "MATCH (n:Person) RETURN"], "", 3),
            (["--query", "CREATE"], "", 3),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut bytes = Vec::new();
            let error = run(&mut db, &contexts, &options(&parts), None, true, &mut bytes)
                .await
                .expect_err("a misrouted or invalid step refuses");
            assert_eq!(error.code, code, "{parts:?}: {}", error.message);
            assert!(
                error.message.starts_with("transaction step 1: "),
                "{}",
                error.message
            );
            assert!(
                error.message.ends_with(expected),
                "{parts:?}: {}",
                error.message
            );
            assert!(bytes.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Every write-with-RETURN form `fgdb write` runs is also a transaction step
/// (fgdb-j72ep): CREATE ... RETURN, MERGE ... ON MATCH / ON CREATE SET ...
/// RETURN and MATCH ... SET ... RETURN in its inline-map and WHERE forms,
/// each returning its row under its statement number and committing the same
/// state, while --rollback returns no row and leaves the state unchanged.
#[test]
fn write_steps_with_return_run_in_a_transaction_and_roll_back_cleanly() {
    let ((), report) = run_async_under_lab(0x7478_6341, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        okay(
            run(
                &mut db,
                &contexts,
                &options(&["--write", "CREATE (n:Person {p: 1})"]),
                None,
                true,
                &mut bytes,
            )
            .await,
        );
        for (steps, rows, values) in [
            (
                vec![
                    "--write",
                    "CREATE (n:Person {p: 7}) RETURN n.p AS p",
                    "--write",
                    "MERGE (n:Person {p: 7}) ON MATCH SET n.p = 8 RETURN n.p AS p",
                ],
                vec![(1, 7), (2, 8)],
                vec![1, 8],
            ),
            (
                vec![
                    "--write",
                    "MATCH (n:Person {p: 1}) SET n.p = 2 RETURN n.p AS p",
                ],
                vec![(1, 2)],
                vec![2, 8],
            ),
            (
                vec![
                    "--write",
                    "MATCH (n:Person) WHERE n.p = 2 SET n.p = 3 RETURN n.p AS p",
                ],
                vec![(1, 3)],
                vec![3, 8],
            ),
            (
                vec![
                    "--write",
                    "MERGE (n:Person {p: 5}) ON CREATE SET n.p = 6 RETURN n.p AS p",
                ],
                vec![(1, 6)],
                vec![3, 6, 8],
            ),
        ] {
            let before = db.frontier().unwrap();
            let mut bytes = Vec::new();
            okay(run(&mut db, &contexts, &options(&steps), None, true, &mut bytes).await);
            let text = output(bytes);
            assert_eq!(
                text.matches("\"event\":\"row\"").count(),
                rows.len(),
                "{text}"
            );
            for (statement, value) in rows {
                let row = format!(
                    r#""statement":{statement},"cells":[{{"type":"int","value":"{value}"}}]"#
                );
                assert!(text.contains(&row), "{steps:?}: {text}");
            }
            assert!(text.contains("\"kind\":\"committed\""), "{text}");
            assert_eq!(db.frontier().unwrap(), CommitSeq(before.0 + 1));
            assert_eq!(committed_values(&db), values, "{steps:?}");
        }

        let before = db.frontier().unwrap();
        let rollback = options(&[
            "--write",
            "MATCH (n:Person {p: 3}) SET n.p = 4 RETURN n.p AS p",
            "--write",
            "MERGE (n:Person {p: 6}) ON MATCH SET n.p = 9 RETURN n.p AS p",
            "--rollback",
        ]);
        let mut bytes = Vec::new();
        okay(run(&mut db, &contexts, &rollback, None, true, &mut bytes).await);
        let text = output(bytes);
        assert!(text.contains("\"kind\":\"rolled_back\""), "{text}");
        assert!(!text.contains("\"event\":\"row\""), "{text}");
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(committed_values(&db), vec![3, 6, 8]);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
