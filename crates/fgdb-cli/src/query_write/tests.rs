use super::*;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, VId};

fn options(parts: &[&str]) -> Options {
    let mut args = vec![
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--label",
        "Person=1",
        "--label",
        "City=2",
        "--property",
        "bornIn=1",
        "--property",
        "name=2",
        "--property",
        "seen=3",
    ];
    args.extend_from_slice(parts);
    let mut options = crate::parse(
        &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        "write",
    )
    .unwrap();
    // The production dispatcher binds exactly these source arguments after
    // resolving the optional time-zone artifact. These tests use no artifact.
    for (name, raw) in &options.raw_params {
        options
            .params
            .insert(name, crate::parameter(raw, None).unwrap())
            .unwrap();
    }
    options
}

#[test]
fn source_parameters_and_feed_declarations_have_separate_native_schemas() {
    let options = options(&[
        "--from-query",
        "RETURN $city AS city",
        "--param",
        "city=text:Paris",
        "--feed-type",
        "city=text",
        "MERGE (c:City {name:$city})",
    ]);
    let prepared = prepare(&options).unwrap().unwrap();
    assert_eq!(prepared.query.source().columns(), ["city"]);
    assert_eq!(prepared.query.script().parameter_schema().len(), 1);
    assert_eq!(
        prepared.query.script().parameter_schema()[0].parameter_type,
        GqlParameterType::Scalar(CanonicalScalarKind::Text),
    );
    // A global read parameter is not implicitly copied into every write row.
    let options = options_with_missing_projection();
    let error = prepare(&options).expect_err("missing script input must refuse");
    assert_eq!(error.code, 3);
    assert!(error.message.contains("MissingColumn"), "{}", error.message);
}

fn options_with_missing_projection() -> Options {
    options(&[
        "--from-query",
        "RETURN $minimum AS unused",
        "--param",
        "minimum=int:1",
        "MERGE (c:City {name:$minimum})",
    ])
}

#[test]
fn definitions_refuse_missing_or_element_columns_and_returning_scripts_before_open() {
    for (source, script) in [
        ("RETURN 1 AS other", "MERGE (c:City {name:$key})"),
        (
            "MATCH (p:Person) RETURN p AS key",
            "MERGE (c:City {name:$key})",
        ),
        ("RETURN 1 AS key", "MERGE (c:City {name:$key}) RETURN c"),
        ("CREATE (:Person)", "CREATE (:City)"),
    ] {
        let options = options(&["--from-query", source, script]);
        assert!(prepare(&options).is_err(), "{source} / {script}");
    }
    let ordinary = options(&["CREATE (:City)"]);
    assert!(prepare(&ordinary).unwrap().is_none());
}

#[test]
fn flags_are_write_only_bounded_and_never_silently_ignored() {
    let bad: &[&[&str]] = &[
        &["--from-query", "RETURN 1 AS key", "--rows", "json:[]"],
        &["--rows", "json:[]", "--from-query", "RETURN 1 AS key"],
        &["--feed-type", "key=int64"],
        &["--max-batch-statements", "0"],
        &[
            "--from-query",
            "RETURN 1 AS key",
            "--from-query",
            "RETURN 2 AS key",
        ],
        &["--from-query", "RETURN 1 AS key", "--feed-type", "key=list"],
        &[
            "--from-query",
            "RETURN 1 AS key",
            "--feed-type",
            "key=text",
            "--feed-type",
            "key=text",
        ],
        &[
            "--from-query",
            "RETURN 1 AS key",
            "--max-batch-statements",
            "-1",
        ],
        &[
            "--from-query",
            "RETURN 1 AS key",
            "--max-batch-statements",
            "1",
            "--max-batch-statements",
            "1",
        ],
    ];
    for extra in bad {
        let mut args = vec!["--db", "unused", "--key-file", "unused"];
        args.extend_from_slice(extra);
        args.push("CREATE (n)");
        let error = crate::parse(
            &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            "write",
        )
        .err()
        .expect("invalid flags must refuse");
        assert_eq!(error.code, 2, "{}", error.message);
    }
    for command in ["query", "transaction", "import-csv", "create", "load"] {
        let args = [
            "--db",
            "unused",
            "--key-file",
            "unused",
            "--from-query",
            "RETURN 1",
        ]
        .map(str::to_owned);
        assert!(crate::parse(&args, command).is_err(), "{command}");
    }
    let mut flags = QueryWriteOptions::default();
    flags.set("--from-query", "RETURN 1 AS key").unwrap();
    let too_many = (PreparedGraphWriteScript::MAX_BATCH_STATEMENTS as u128 + 1).to_string();
    assert!(flags.set("--max-batch-statements", &too_many).is_err());
    flags.set("--max-batch-statements", "0").unwrap();
    assert_eq!(flags.max_statements, Some(0));
    assert!(flags.validate(false).is_ok());
}

#[test]
fn output_failure_after_completion_does_not_retry_or_undo_the_write() {
    struct BrokenOutput;
    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected terminal transport failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let keys = DatabaseKeys::new(
            [0x41; 32],
            DatabaseSecurityNamespaceId([0x42; 32]),
            [0x43; 32],
        );
        let mut db = Database::open_memory(&contexts.commit(), keys)
            .await
            .unwrap();
        // The source has a wide identity, but only its projected scalar binds.
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(
            VId((1_u128 << 100) | 1),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        db.write(&contexts.commit(), seed).await.unwrap();
        let before = db.frontier().unwrap();
        let options = options(&[
            "--from-query",
            "MATCH (p:Person) RETURN p.bornIn AS key",
            "CREATE (c:City {name:$key})",
        ]);
        let prepared = prepare(&options).unwrap().unwrap();
        let error = run(&mut db, &contexts, prepared, true, &mut BrokenOutput)
            .await
            .expect_err("transport must fail");
        assert_eq!(error.code, 5);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
}
