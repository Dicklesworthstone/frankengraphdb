//! Decimal values emitted by the server can be supplied back as typed operands.
//! These tests exercise the real codec/binder and shared authorized dispatcher.

use super::*;
use fgdb_types::{CanonicalDecimal, MAX_DECIMAL_COEFFICIENT, MIN_DECIMAL_COEFFICIENT};

fn wire(coefficient: i128) -> WireValue {
    WireValue::Decimal(
        CanonicalDecimal::from_coefficient(coefficient)
            .unwrap()
            .to_string(),
    )
}

#[test]
fn scalar_arguments_preserve_exact_coefficients_and_output_spelling() {
    for coefficient in [
        MIN_DECIMAL_COEFFICIENT,
        -1,
        0,
        1,
        9_007_199_254_740_993_000_000_000_000_000_001,
        MAX_DECIMAL_COEFFICIENT,
    ] {
        let input = wire(coefficient);
        let bound = parameters(&[("amount".into(), input.clone())], None).unwrap();
        let Some(GqlParameterValue::Scalar(value)) = bound.get("amount") else {
            panic!("decimal must bind as its own canonical scalar kind");
        };
        assert_eq!(
            value.value(),
            &CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(coefficient).unwrap())
        );
        assert_eq!(scalar_value(value.value()), input);
        assert_eq!(bound.canonical_bytes().len(), bound.canonical_byte_len());
    }
}

#[test]
fn nested_decimal_maps_and_lists_use_the_same_exact_scalar_codec() {
    let input = WireValue::Map(vec![
        ("amount".into(), wire(MAX_DECIMAL_COEFFICIENT)),
        (
            "items".into(),
            WireValue::List(vec![
                wire(1),
                wire(-1),
                WireValue::Null,
                WireValue::Map(vec![("refund".into(), wire(MIN_DECIMAL_COEFFICIENT))]),
            ]),
        ),
    ]);
    let bound = parameters(&[("payload".into(), input.clone())], None).unwrap();
    let Some(GqlParameterValue::Map(value)) = bound.get("payload") else {
        panic!("map parameter required");
    };
    assert_eq!(graph(value.value()), input);
    let list = WireValue::List(vec![input, wire(0)]);
    let bound = parameters(&[("rows".into(), list.clone())], None).unwrap();
    let Some(GqlParameterValue::List(value)) = bound.get("rows") else {
        panic!("list parameter required");
    };
    assert_eq!(graph(value.value()), list);
}

#[test]
fn bad_decimal_text_refuses_whole_arguments_without_exposing_nested_values() {
    for text in [
        "private-decimal-payload",
        "1.23",
        "-0.000000000000000000",
        "01.000000000000000000",
        "10000000000000000.000000000000000000",
        "0.0000000000000000001",
        "NaN",
        "1e18",
    ] {
        for bad in [
            WireValue::Decimal(text.into()),
            WireValue::List(vec![wire(1), WireValue::Decimal(text.into())]),
            WireValue::Map(vec![(
                "private-key".into(),
                WireValue::Decimal(text.into()),
            )]),
        ] {
            let error = parameters(&[("a".into(), wire(1)), ("z".into(), bad)], None).unwrap_err();
            assert_eq!(error.parameter, "z");
            assert_eq!(
                error.reason,
                "a decimal argument must be canonical fixed-scale text"
            );
            assert!(!error.to_string().contains(text));
            assert!(!error.to_string().contains("private-key"));
        }
    }
    // This feature does not reinterpret ordinary text or widen aggregate IDs.
    let bound = parameters(&[("text".into(), WireValue::Text("1.23".into()))], None).unwrap();
    assert!(
        matches!(bound.get("text"), Some(GqlParameterValue::Scalar(value))
        if matches!(value.value(), CanonicalScalar::Text(_)))
    );
    assert!(parameters(&[("bad".into(), WireValue::WideInt(1))], None).is_err());
}

#[test]
fn json_decimal_tags_share_nested_native_binding_and_canonical_refusals() {
    use fgdb_protocol::json::{argument, parse_json};

    let input = parse_json(
        r#"[{"$decimal":"9007199254740993.000000000000000001"},
            {"amount":{"$decimal":"-0.000000000000000001"}}]"#,
        32,
        64,
    )
    .unwrap();
    let bound = parameters(&[("rows".into(), argument(&input).unwrap())], None).unwrap();
    let Some(GqlParameterValue::List(value)) = bound.get("rows") else {
        panic!("JSON array must retain its native list type");
    };
    assert_eq!(
        graph(value.value()),
        WireValue::List(vec![
            wire(9_007_199_254_740_993_000_000_000_000_000_001),
            WireValue::Map(vec![("amount".into(), wire(-1))]),
        ])
    );
    for input in [
        r#"{"$decimal":"1.25"}"#,
        r#"[{"$decimal":"-0.000000000000000000"}]"#,
        r#"{"amount":{"$decimal":"10000000000000000.000000000000000000"}}"#,
    ] {
        let value = argument(&parse_json(input, 32, 64).unwrap()).unwrap();
        let error = parameters(&[("payload".into(), value)], None).unwrap_err();
        assert_eq!(error.parameter, "payload");
        assert_eq!(
            error.reason,
            "a decimal argument must be canonical fixed-scale text"
        );
    }
}

mod served {
    use super::*;
    use crate::execute::{self, Answer, Refusal};
    use crate::recovery::tests::served;
    use crate::{Server, TRUNK, unix_millis};
    use asupersync::{Cx, lab::run_async_under_lab};
    use fgdb::{Database, DatabaseKeys};
    use fgdb_gql::GraphSymbolKind;
    use fgdb_protocol::body::{Body, ErrorCode, Execute, ExecuteMode, Outcome, ResultChunk};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use fgdb_warden::{CapabilityToken, Grant, QueryLimits, Rights, Scope};
    use std::path::PathBuf;
    use std::sync::Arc;

    async fn fixture(cx: &Cx, name: &str) -> (Server, CapabilityToken, PathBuf) {
        let (mut server, token, path) = served(cx, name).await;
        let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        db.symbols.bind(GraphSymbolKind::Label, "Item", 1).unwrap();
        db.symbols
            .bind(GraphSymbolKind::Property, "amount", 1)
            .unwrap();
        (server, token, path)
    }

    fn accepted(result: Result<Answer, Refusal>) -> Answer {
        result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
    }

    fn refused(result: Result<Answer, Refusal>) -> Refusal {
        match result {
            Ok(_) => panic!("invalid or unauthorized decimal write must refuse"),
            Err(error) => error,
        }
    }

    fn request(mode: ExecuteMode, text: &str, parameters: Vec<(String, WireValue)>) -> Execute {
        let request = Execute {
            mode,
            statement: text.into(),
            parameters,
        };
        // The actual FGP typed body, not interpolated decimal query text.
        Execute::decode(&request.encode().unwrap()).unwrap()
    }

    #[test]
    fn json_decimal_ingestion_rejects_a_bad_tail_then_commits_both_exact_values() {
        use fgdb_protocol::json::{Json, argument, parse_json, quote};

        let ((), report) = run_async_under_lab(0xd3c1_0004, |root| async move {
            let (server, token, _) = fixture(&root, "decimal-json-ingestion").await;
            let db = &server.databases["test"];
            let before = db.db.read(&root).await.unwrap().frontier().unwrap();
            let arguments = |text: &str| {
                let Json::Object(fields) = parse_json(text, 32, 128).unwrap() else {
                    panic!("HTTP parameters are a JSON object");
                };
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), argument(value).unwrap()))
                    .collect()
            };
            let script = "CREATE (a:Item {amount:$a}); CREATE (b:Item {amount:$b})";
            let error = refused(
                execute::write(
                    &root,
                    db,
                    &token,
                    &request(
                        ExecuteMode::Write,
                        script,
                        arguments(
                            r#"{"a":{"$decimal":"0.000000000000000001"},
                                      "b":{"$decimal":"1.25"}}"#,
                        ),
                    ),
                )
                .await,
            );
            assert_eq!(error.code, ErrorCode::Statement);
            assert!(!error.message.contains("1.25"));
            let guard = db.db.read(&root).await.unwrap();
            assert_eq!(guard.frontier().unwrap(), before);
            assert!(guard.vertices().unwrap().is_empty());
            drop(guard);
            let answer = accepted(
                execute::write(
                    &root,
                    db,
                    &token,
                    &request(
                        ExecuteMode::Write,
                        script,
                        arguments(
                            r#"{"a":{"$decimal":"9007199254740993.000000000000000001"},
                                      "b":{"$decimal":"-0.000000000000000001"}}"#,
                        ),
                    ),
                )
                .await,
            );
            assert!(matches!(answer.outcome,
                Outcome::WriteCommitted { seq, statements: 2 } if seq == before.0 + 1
            ));
            assert_eq!(
                db.db.read(&root).await.unwrap().vertices().unwrap().len(),
                2
            );
            for amount in [
                "9007199254740993.000000000000000001",
                "-0.000000000000000001",
            ] {
                let params = format!(r#"{{"amount":{{"$decimal":{}}}}}"#, quote(amount));
                let answer = accepted(
                    execute::read(
                        &root,
                        db,
                        &token,
                        &request(
                            ExecuteMode::Read,
                            "MATCH (n:Item) WHERE n.amount=$amount RETURN n.amount AS amount",
                            arguments(&params),
                        ),
                    )
                    .await,
                );
                assert_eq!(answer.rows, vec![vec![WireValue::Decimal(amount.into())]]);
            }
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn decimal_create_match_and_update_round_trip_through_native_storage() {
        let ((), report) = run_async_under_lab(0xd3c1_0001, |root| async move {
            let (server, token, path) = fixture(&root, "decimal-native-storage").await;
            let db = &server.databases["test"];
            for coefficient in [
                1,
                -1,
                9_007_199_254_740_993_000_000_000_000_000_001,
                MAX_DECIMAL_COEFFICIENT,
            ] {
                let input = wire(coefficient);
                let created = accepted(
                    execute::write(
                        &root,
                        db,
                        &token,
                        &request(
                            ExecuteMode::Write,
                            "CREATE (n:Item {amount:$amount}) RETURN n.amount AS amount",
                            vec![("amount".into(), input.clone())],
                        ),
                    )
                    .await,
                );
                assert_eq!(created.columns, vec!["amount"]);
                assert_eq!(created.rows, vec![vec![input.clone()]]);
                assert!(matches!(
                    created.outcome,
                    Outcome::WriteCommitted { statements: 1, .. }
                ));
                let queried = accepted(
                    execute::read(
                        &root,
                        db,
                        &token,
                        &request(
                            ExecuteMode::Read,
                            "MATCH (n:Item) WHERE n.amount=$amount RETURN n.amount AS amount",
                            vec![("amount".into(), input.clone())],
                        ),
                    )
                    .await,
                );
                assert_eq!(queried.rows, vec![vec![input]]);
                let chunk = ResultChunk {
                    columns: Some(queried.columns),
                    rows: queried.rows,
                };
                assert_eq!(
                    ResultChunk::decode(&chunk.encode().unwrap()).unwrap(),
                    chunk
                );
            }
            let updated = accepted(
                execute::write(
                    &root,
                    db,
                    &token,
                    &request(
                        ExecuteMode::Write,
                        "MATCH (n:Item) WHERE n.amount=$amount \
                         SET n.amount=$replacement RETURN n.amount AS amount",
                        vec![
                            ("amount".into(), wire(1)),
                            ("replacement".into(), wire(MIN_DECIMAL_COEFFICIENT)),
                        ],
                    ),
                )
                .await,
            );
            assert_eq!(updated.rows, vec![vec![wire(MIN_DECIMAL_COEFFICIENT)]]);
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut guard = db.db.write(&root).await.unwrap();
            let expected = guard.vertices().unwrap();
            let frontier = guard.frontier().unwrap();
            guard.compact(&contexts.commit()).await.unwrap();
            drop(guard);
            server.join_database_workers(&root).await;
            drop(server);
            // Match the existing recovery fixture's actual key set.
            let keys = DatabaseKeys::new(
                [0x91; 32],
                DatabaseSecurityNamespaceId([0x92; 32]),
                [0x93; 32],
            );
            let reopened = Database::open(&contexts.commit(), &path, keys)
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), expected);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn malformed_late_decimal_argument_cannot_commit_an_earlier_script_statement() {
        let ((), report) = run_async_under_lab(0xd3c1_0002, |root| async move {
            let (server, token, _) = fixture(&root, "decimal-bad-tail").await;
            let db = &server.databases["test"];
            let before = db.db.read(&root).await.unwrap().frontier().unwrap();
            let text = "CREATE (a:Item {amount:$a}); CREATE (b:Item {amount:$z})";
            let error = refused(
                execute::write(
                    &root,
                    db,
                    &token,
                    &request(
                        ExecuteMode::Write,
                        text,
                        vec![
                            ("a".into(), wire(1)),
                            ("z".into(), WireValue::Decimal("1.23".into())),
                        ],
                    ),
                )
                .await,
            );
            assert_eq!(error.code, ErrorCode::Statement);
            assert!(!error.message.contains("1.23"));
            let guard = db.db.read(&root).await.unwrap();
            assert_eq!(guard.frontier().unwrap(), before);
            assert!(guard.vertices().unwrap().is_empty());
            drop(guard);
            let answer = accepted(
                execute::write(
                    &root,
                    db,
                    &token,
                    &request(
                        ExecuteMode::Write,
                        text,
                        vec![("a".into(), wire(1)), ("z".into(), wire(-1))],
                    ),
                )
                .await,
            );
            assert!(matches!(answer.outcome,
                Outcome::WriteCommitted { seq, statements: 2 } if seq == before.0 + 1
            ));
            assert_eq!(
                db.db.read(&root).await.unwrap().vertices().unwrap().len(),
                2
            );
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn accepting_decimal_operands_does_not_grant_write_or_property_authority() {
        let ((), report) = run_async_under_lab(0xd3c1_0003, |root| async move {
            let (server, _, _) = fixture(&root, "decimal-authority").await;
            let db = &server.databases["test"];
            let before = db.db.read(&root).await.unwrap().frontier().unwrap();
            for forbidden_property in [false, true] {
                let token = db
                    .authority
                    .issue_at(
                        &Grant {
                            branch: TRUNK.into(),
                            labels: Scope::All,
                            relations: Scope::All,
                            properties: if forbidden_property {
                                Scope::only([fgdb_delta_types::PropertyKeyId(99)])
                            } else {
                                Scope::All
                            },
                            rights: if forbidden_property {
                                Rights::ReadWrite
                            } else {
                                Rights::Read
                            },
                            limits: QueryLimits {
                                max_nodes: 1_000_000,
                                max_work: 100_000_000,
                                max_rows: 100,
                            },
                            expires_at_ms: u64::MAX / 2,
                        },
                        unix_millis(),
                    )
                    .unwrap();
                let error = refused(
                    execute::write(
                        &root,
                        db,
                        &token,
                        &request(
                            ExecuteMode::Write,
                            "CREATE (n:Item {amount:$amount}) RETURN n.amount AS amount",
                            vec![("amount".into(), wire(MAX_DECIMAL_COEFFICIENT))],
                        ),
                    )
                    .await,
                );
                assert_eq!(error.code, ErrorCode::PermissionDenied);
                let guard = db.db.read(&root).await.unwrap();
                assert_eq!(guard.frontier().unwrap(), before);
                assert!(guard.vertices().unwrap().is_empty());
            }
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
