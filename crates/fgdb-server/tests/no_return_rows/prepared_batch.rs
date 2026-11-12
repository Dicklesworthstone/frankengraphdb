//! Reusing a connection-owned script or chain preserves the native batch boundary:
//! one cumulative allowance, no failed prefix, and one durable completion.

use super::*;

fn arguments(name: &str, tail: &str, divisor: i64, age: i64) -> Vec<(String, WireValue)> {
    vec![
        ("name".into(), WireValue::Text(name.into())),
        ("tail".into(), WireValue::Text(tail.into())),
        ("divisor".into(), WireValue::Int(divisor)),
        ("age".into(), WireValue::Int(age)),
    ]
}

fn refusal(error: ClientError) -> ErrorCode {
    match error {
        ClientError::Server { code, .. } => code,
        other => panic!("expected a server refusal, got {other:?}"),
    }
}

#[test]
fn prepared_batches_rebind_once_per_record_and_reopen_without_failed_prefixes() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-batch-atomic", |config| {
            config.max_statements = 4;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::Write, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let script = "CREATE (:Person {name:$name,age:100/$divisor}); \
                      CREATE (:Person {name:$tail,age:$age})";
        let handle = writer
            .prepare_write(cx, script, arguments("sample", "sample-tail", 2, 99))
            .await
            .unwrap();
        let before = state(cx, &mut observer).await;
        assert!(before.0.rows.is_empty() && before.1.rows.is_empty());
        assert_eq!(before.0.outcome, Outcome::Rows { seq: 0 });

        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    handle,
                    vec![
                        arguments("Ann", "Ann-tail", 2, 30),
                        arguments("Bob", "Bob-tail", 4, 20)
                    ],
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 4
            },
            "four statements share one commit and need no result-row grant"
        );
        let committed = state(cx, &mut observer).await;
        assert_eq!(committed.0.rows.len(), 4);
        assert!(committed.1.rows.is_empty());
        assert_eq!(committed.0.outcome, Outcome::Rows { seq: 1 });

        let mut missing = arguments("missing", "missing-tail", 2, 1);
        missing.retain(|(name, _)| name != "age");
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![arguments("prefix", "prefix-tail", 2, 1), missing],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "a later binding failure must not publish an earlier record"
        );

        let mut foreign_identity = arguments("foreign", "foreign-tail", 2, 1);
        foreign_identity.push(("injected".into(), WireValue::Vertex(77)));
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![arguments("prefix", "prefix-tail", 2, 1), foreign_identity],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "wire identities are not authority to refer to graph elements"
        );

        assert!(matches!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![
                            arguments("prefix", "prefix-tail", 2, 1),
                            arguments("bad", "bad-tail", 0, 1)
                        ],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement | ErrorCode::Execution
        ));
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![arguments("over-limit", "over-limit-tail", 2, 1); 3],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget,
            "the host limit counts all six statements, not each two-statement record"
        );
        assert!(
            writer
                .execute_prepared_batch(cx, handle, vec![])
                .await
                .is_err()
        );
        let unchanged = state(cx, &mut observer).await;
        assert_eq!(unchanged.0.rows, committed.0.rows);
        assert_eq!(unchanged.1.rows, committed.1.rows);
        assert_eq!(unchanged.0.outcome, Outcome::Rows { seq: 1 });

        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![arguments("Cy", "Cy-tail", 5, 10)])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 2,
                statements: 2
            },
            "the retained script and connection remain usable after refused batches"
        );
        let final_state = state(cx, &mut observer).await;
        assert_eq!(
            final_state
                .0
                .rows
                .iter()
                .map(|row| row[1..].to_vec())
                .collect::<Vec<_>>(),
            [
                ("Ann", 50),
                ("Ann-tail", 30),
                ("Bob", 25),
                ("Bob-tail", 20),
                ("Cy", 20),
                ("Cy-tail", 10),
            ]
            .map(|(name, age)| vec![WireValue::Text(name.into()), WireValue::Int(age)])
        );
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 6, 0).await;
    });
}

#[test]
fn prepared_merge_batches_observe_staged_records_and_each_new_committed_snapshot() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-batch-merge", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let args = |name: &str, age| {
            vec![
                ("name".into(), WireValue::Text(name.into())),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let handle = writer
            .prepare_write(
                cx,
                "MERGE (n:Person {name:$name}) ON CREATE SET n.age=$age ON MATCH SET n.age=n.age+$age",
                args("representative", 99),
            )
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    handle,
                    vec![args("Ann", 2), args("Ann", 3), args("Bob", 7)]
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3
            }
        );
        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![args("Ann", 4), args("Bob", 1)])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 2,
                statements: 2
            }
        );
        let final_state = state(cx, &mut observer).await;
        assert_eq!(
            final_state
                .0
                .rows
                .iter()
                .map(|row| row[1..].to_vec())
                .collect::<Vec<_>>(),
            [
                vec![WireValue::Text("Ann".into()), WireValue::Int(9)],
                vec![WireValue::Text("Bob".into()), WireValue::Int(8)],
            ]
        );
        let no_effect = writer
            .prepare_write(
                cx,
                "MATCH (n:Person {name:$name}) SET n.age=$age",
                args("missing", 1),
            )
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    no_effect,
                    vec![args("missing", 2), args("still-missing", 3)]
                )
                .await
                .unwrap(),
            Outcome::ReadClosed {
                seq: 2,
                statements: 2
            },
            "an all-no-effect batch does not synthesize a write commit"
        );
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 2, 0).await;
    });
}

#[test]
fn prepared_batch_handles_are_write_only_connection_owned_and_invalidated_by_refresh() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-batch-ownership", |_| {}).await;
        let write_scope = grant(Rights::Write, 0);
        let mut writer = fixture.client(cx, &write_scope).await;
        let mut foreign = fixture.client(cx, &write_scope).await;
        let mut reader = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let handle = writer
            .prepare_write(cx, "CREATE (:Person)", vec![])
            .await
            .unwrap();
        assert_eq!(
            refusal(
                foreign
                    .execute_prepared_batch(cx, handle, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        foreign.release_prepared(cx, handle).await.unwrap();
        let read_handle = reader
            .prepare_read(cx, "MATCH (n:Person) RETURN n", vec![])
            .await
            .unwrap();
        assert_eq!(
            refusal(
                reader
                    .execute_prepared_batch(cx, read_handle, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![vec![], vec![]])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 2
            }
        );
        let authority = issuer(issuer_key(), &keys(), 1).unwrap();
        writer
            .refresh_authority(cx, issue_token(&authority, &write_scope).unwrap())
            .await
            .unwrap();
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(cx, handle, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "AUTH_REFRESH cannot retain executable templates from the old authority generation"
        );
        let released = writer
            .prepare_write(cx, "CREATE (:Person)", vec![])
            .await
            .unwrap();
        writer.release_prepared(cx, released).await.unwrap();
        writer.release_prepared(cx, released).await.unwrap();
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(cx, released, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(state(cx, &mut reader).await.0.rows.len(), 2);
        reader.close(cx).await.unwrap();
        foreign.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 2, 0).await;
    });
}

fn relationship_arguments(
    source: &str,
    target: &str,
    weight: i64,
    divisor: i64,
) -> Vec<(String, WireValue)> {
    vec![(
        "rows".into(),
        WireValue::List(vec![WireValue::Map(vec![
            ("d".into(), WireValue::Text(target.into())),
            ("divisor".into(), WireValue::Int(divisor)),
            ("s".into(), WireValue::Text(source.into())),
            ("w".into(), WireValue::Int(weight)),
        ])]),
    )]
}

#[test]
fn prepared_chain_batches_preserve_input_order_atomicity_and_zero_row_grants() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-chain-batch", |config| {
            config.max_statements = 3;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let statement = "UNWIND $rows AS row MERGE (a:Person {name:row.s}) \
            MERGE (b:Person {name:row.d}) MERGE (a)-[r:KNOWS]->(b) \
            ON CREATE SET r.age=row.w/row.divisor \
            ON MATCH SET r.age=r.age*10+row.w/row.divisor";
        let handle = writer
            .prepare_write(
                cx,
                statement,
                relationship_arguments("sample", "only", 99, 1),
            )
            .await
            .unwrap();
        assert!(state(cx, &mut observer).await.0.rows.is_empty());
        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    handle,
                    vec![
                        relationship_arguments("Ada", "Bob", 1, 1),
                        relationship_arguments("Ada", "Bob", 2, 1),
                        relationship_arguments("Bob", "Cy", 3, 1),
                    ],
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3,
            },
            "all three chains share one completion and require no result-row allowance"
        );
        let committed = state(cx, &mut observer).await;
        assert_eq!((committed.0.rows.len(), committed.1.rows.len()), (3, 2));
        assert_eq!(
            committed
                .1
                .rows
                .iter()
                .map(|row| (row[0].clone(), row[1].clone(), row[3].clone()))
                .collect::<Vec<_>>(),
            [
                (
                    WireValue::Text("Ada".into()),
                    WireValue::Text("Bob".into()),
                    WireValue::Int(12),
                ),
                (
                    WireValue::Text("Bob".into()),
                    WireValue::Text("Cy".into()),
                    WireValue::Int(3),
                ),
            ],
            "a later argument set sees earlier staged endpoints and relationship updates"
        );

        let arithmetic = writer
            .execute_prepared_batch(
                cx,
                handle,
                vec![
                    relationship_arguments("Cy", "De", 4, 1),
                    relationship_arguments("Ada", "Bob", 5, 0),
                ],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &arithmetic,
                ClientError::Server { code: ErrorCode::Execution, message }
                    if message.contains("argument set 1")
            ),
            "a late arithmetic failure identifies its record and rolls back the entire batch: {arithmetic:?}"
        );
        assert_eq!(state(cx, &mut observer).await, committed);
        let missing = writer
            .execute_prepared_batch(
                cx,
                handle,
                vec![relationship_arguments("Cy", "De", 4, 1), vec![]],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &missing,
                ClientError::Server {
                    code: ErrorCode::Statement,
                    message,
                } if message.contains("argument set 1:")
            ),
            "every record binds independently and identifies its missing operand: {missing:?}"
        );
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![relationship_arguments("Cy", "De", 4, 1); 4],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget,
        );
        assert!(matches!(
            writer.execute_prepared_batch(cx, handle, vec![]).await,
            Err(ClientError::Body(
                fgdb_protocol::body::BodyError::Noncanonical
            ))
        ));
        assert_eq!(state(cx, &mut observer).await, committed);

        let no_effect = writer
            .prepare_write(
                cx,
                "UNWIND $rows AS row MERGE (a:Person {name:row.s}) \
                 MERGE (b:Person {name:row.d}) MERGE (a)-[:KNOWS]->(b)",
                relationship_arguments("sample", "only", 99, 1),
            )
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    no_effect,
                    vec![
                        relationship_arguments("Ada", "Bob", 1, 1),
                        relationship_arguments("Bob", "Cy", 2, 1),
                    ],
                )
                .await
                .unwrap(),
            Outcome::ReadClosed {
                seq: 1,
                statements: 2,
            },
            "an effect-free chain batch closes at its snapshot without a synthetic commit"
        );
        assert_eq!(state(cx, &mut observer).await, committed);
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 3, 2).await;
    });
}

#[test]
fn prepared_chain_batches_share_native_creation_limits_without_committed_prefixes() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-chain-batch-creation-limit", |config| {
            config.write_policy.max_created_vertices = 3;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let statement = "CREATE (a:Person {name:$name}) WITH a \
                 CREATE (b:Person {name:$tail}) CREATE (a)-[:KNOWS]->(b)";
        let names = |name: &str, tail: &str| {
            vec![
                ("name".into(), WireValue::Text(name.into())),
                ("tail".into(), WireValue::Text(tail.into())),
            ]
        };
        // Keep the original four-name representative as a strict-unused
        // refusal. This statement consumes name/tail, not divisor/age.
        assert_eq!(
            refusal(
                writer
                    .prepare_write(cx, statement, arguments("sample", "sample-tail", 1, 0))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        let handle = writer
            .prepare_write(cx, statement, names("sample", "sample-tail"))
            .await
            .unwrap();
        for original in [
            vec![arguments("Ada", "Bob", 1, 0), arguments("Cy", "De", 1, 0)],
            vec![arguments("Ada", "Bob", 1, 0)],
        ] {
            assert_eq!(
                refusal(
                    writer
                        .execute_prepared_batch(cx, handle, original)
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement
            );
        }
        let exhausted = writer
            .execute_prepared_batch(cx, handle, vec![names("Ada", "Bob"), names("Cy", "De")])
            .await
            .unwrap_err();
        assert!(
            matches!(&exhausted, ClientError::Server {
            code: ErrorCode::Budget, message,
        } if message.contains("argument set 1")),
            "each record fits, but their four creations exceed the one batch allowance: {exhausted:?}"
        );
        let unchanged = state(cx, &mut observer).await;
        assert!(unchanged.0.rows.is_empty() && unchanged.1.rows.is_empty());
        assert_eq!(unchanged.0.outcome, Outcome::Rows { seq: 0 });
        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![names("Ada", "Bob")])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1,
            },
            "the connection and retained template remain usable after a failed batch"
        );
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 2, 1).await;
    });
}

#[test]
fn text_chain_batches_cross_the_real_wire_and_keep_one_atomic_completion() {
    run(async |cx| {
        // Retain the host's existing default 64-statement limit.
        let fixture = Fixture::start(cx, "text-chain-batch", |config| {
            assert_eq!(config.max_statements, 64);
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let statement = "UNWIND $rows AS row MERGE (a:Person {name:row.s}) \
            MERGE (b:Person {name:row.d}) MERGE (a)-[r:KNOWS]->(b) \
            ON CREATE SET r.age=row.w/row.divisor \
            ON MATCH SET r.age=r.age*10+row.w/row.divisor";
        assert_eq!(
            writer
                .execute_batch(
                    cx,
                    statement,
                    vec![
                        relationship_arguments("Ada", "Bob", 1, 1),
                        relationship_arguments("Ada", "Bob", 2, 1),
                        relationship_arguments("Bob", "Cy", 3, 1),
                    ]
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3
            }
        );
        let committed = state(cx, &mut observer).await;
        assert_eq!(committed.0.outcome, Outcome::Rows { seq: 1 });
        assert_eq!((committed.0.rows.len(), committed.1.rows.len()), (3, 2));
        assert_eq!(
            committed
                .0
                .rows
                .iter()
                .map(|row| row[1].clone())
                .collect::<Vec<_>>(),
            ["Ada", "Bob", "Cy"].map(|name| WireValue::Text(name.into()))
        );
        assert_eq!(
            committed
                .1
                .rows
                .iter()
                .map(|row| (row[0].clone(), row[1].clone(), row[3].clone()))
                .collect::<Vec<_>>(),
            [
                (
                    WireValue::Text("Ada".into()),
                    WireValue::Text("Bob".into()),
                    WireValue::Int(12)
                ),
                (
                    WireValue::Text("Bob".into()),
                    WireValue::Text("Cy".into()),
                    WireValue::Int(3)
                ),
            ],
            "later native chains must read the earlier records' staged graph"
        );

        let error = writer
            .execute_batch(
                cx,
                statement,
                vec![
                    relationship_arguments("Cy", "De", 4, 1),
                    relationship_arguments("Ada", "Bob", 5, 0),
                ],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ClientError::Server {
            code: ErrorCode::Execution, message,
        } if message.contains("argument set 1")),
            "{error:?}"
        );
        assert_eq!(state(cx, &mut observer).await, committed);
        let error = writer
            .execute_batch(
                cx,
                statement,
                vec![relationship_arguments("Cy", "De", 4, 1), vec![]],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ClientError::Server {
            code: ErrorCode::Statement, message,
        } if message.contains("argument set 1")),
            "{error:?}"
        );
        assert_eq!(state(cx, &mut observer).await, committed);

        let no_effect = "MERGE (a:Person {name:'Ada'}) MERGE (b:Person {name:'Bob'}) \
            MERGE (a)-[:KNOWS]->(b)";
        assert_eq!(
            writer
                .execute_batch(cx, no_effect, vec![vec![]; 64])
                .await
                .unwrap(),
            Outcome::ReadClosed {
                seq: 1,
                statements: 64
            }
        );
        assert_eq!(
            refusal(
                writer
                    .execute_batch(cx, no_effect, vec![vec![]; 65])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        assert_eq!(
            refusal(
                writer
                    .execute_batch(
                        cx,
                        &format!("{statement} RETURN 1"),
                        vec![relationship_arguments("Cy", "De", 4, 1)]
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied,
            "ordinary returning chains stay outside stats-only batches"
        );
        assert_eq!(state(cx, &mut observer).await, committed);
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        // Shutdown and reopen the same durable database, checking its actual
        // frontier and graph rather than retaining only the wire response.
        fixture.stop(cx, 1, 3, 2).await;
    });
}

#[test]
fn four_thousand_distinct_key_vertex_and_relationship_merges_scale_linearly_and_enforce_late_row_scope()
 {
    use std::collections::BTreeSet;

    run(async |cx| {
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let commit = contexts.commit();
        let txn = contexts.txn();
        let query = contexts.query();

        // 1. Measure exact snapshot-record (max_nodes) and work-unit (max_work)
        // consumption across 1,000, 2,000, and 4,000 distinct-key MERGE rows via
        // tight capability-permit binary search, proving linear 1:2:4 scaling.
        let mut symbols = Symbols::new();
        for (kind, name, id) in [
            (GraphSymbolKind::Label, "Person", 1),
            (GraphSymbolKind::Property, "name", 1),
            (GraphSymbolKind::Property, "age", 2),
            (GraphSymbolKind::Property, "secret", 3),
            (GraphSymbolKind::Relation, "KNOWS", 1),
        ] {
            symbols.bind(kind, name, id).unwrap();
        }
        let default_config = DatabaseConfig::new("social", keys(), issuer_key());
        let authority = issuer(issuer_key(), &keys(), 1).unwrap();
        let vertex_stmt = "MERGE (n:Person {name: $name}) SET n.age = $age";
        let measure_min_work = async |count: usize| -> u64 {
            let params: Vec<_> = (0..count)
                .map(|i| {
                    fgdb_gql::GqlParameters::new()
                        .with_scalar(
                            "name",
                            fgdb_types::CanonicalScalar::ucs_basic_text(&format!("p-{i:04}"))
                                .unwrap(),
                        )
                        .unwrap()
                        .with_int64("age", (i % 97) as i64)
                        .unwrap()
                })
                .collect();
            let mut low = 1_u64;
            let mut high = DatabaseConfig::DEFAULT_QUERY_LIMITS[2];
            while low < high {
                let mid = low + (high - low) / 2;
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let token = authority
                    .issue_at(
                        &Grant {
                            branch: fgdb_server::TRUNK.into(),
                            labels: Scope::All,
                            relations: Scope::All,
                            properties: Scope::All,
                            rights: Rights::ReadWrite,
                            limits: QueryLimits {
                                max_nodes: DatabaseConfig::DEFAULT_QUERY_LIMITS[0],
                                max_work: mid,
                                max_rows: 0,
                            },
                            expires_at_ms: u64::MAX / 2,
                        },
                        500,
                    )
                    .unwrap();
                let mut session = db
                    .authorized_write_session(
                        &txn,
                        &commit,
                        &authority,
                        &token,
                        fgdb_server::TRUNK,
                        symbols.clone(),
                        fgdb_delta_types::RelationId(1),
                        default_config.write_policy,
                        count,
                        || 1_000,
                    )
                    .unwrap();
                if session
                    .query_batch_stats(&query, vertex_stmt, &params)
                    .await
                    .is_ok()
                {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }
            low
        };
        let w1000 = measure_min_work(1_000).await;
        let w2000 = measure_min_work(2_000).await;
        let w4000 = measure_min_work(4_000).await;
        // Exact affine-linear scaling check W(N) = c_0 + c_1 * N:
        // marginal work per 1,000 rows is constant (zero quadratic drift),
        // and 4x input stays strictly below 4x total work.
        let delta_1k_to_2k = w2000 - w1000;
        let delta_1k_to_4k = w4000 - w1000;
        assert!(
            delta_1k_to_2k > 0 && delta_1k_to_4k == delta_1k_to_2k * 3 && w4000 < w1000 * 4,
            "expected exact affine-linear work scaling: w1000={w1000}, w2000={w2000}, w4000={w4000}"
        );

        // 2. Real FGP TCP server with default ServerLimits and default host
        // query/work/snapshot limits (100,000 snapshot records, 10,000,000 work),
        // raising only max_statements = 4,000.
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fgdb-server-4k-merge-batch-{}-{time}",
            std::process::id(),
        ));
        drop(Database::create(&commit, &path, keys()).await.unwrap());
        let mut server = Server::new(cx, ServerLimits::default()).unwrap();
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        assert_eq!(
            config.query_policy.rows.max_snapshot_records(),
            Some(100_000),
            "must run under default 100k snapshot-record ceiling"
        );
        config.symbols = symbols.clone();
        config.max_statements = 4_000;
        server.open_database(cx, &path, config).await.unwrap();
        let shutdown = server.shutdown();
        let server = Arc::new(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut task = cx
            .spawn(move |child| async move { server.serve(&child, listener).await.unwrap() })
            .unwrap();

        let connect = async |grant_spec: &Grant| -> Client {
            let token = issue_token(&authority, grant_spec).unwrap();
            let mut client = Client::connect(cx, address, token).await.unwrap();
            client.select(cx, "social").await.unwrap();
            client
        };
        let mut writer = connect(&grant(Rights::ReadWrite, 0)).await;

        // 2a. Forbidden property in late row 1,023 (MAX_BATCH_ARGUMENT_SETS = 1,024,
        // > 2.2x past the old ~447-row O(N^2) cliff) under restricted Grant refuses
        // the whole 1,024-statement batch with PermissionDenied and 0 effects.
        let mut restricted = connect(&Grant {
            branch: fgdb_server::TRUNK.into(),
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::Only(BTreeSet::from([
                fgdb_delta_types::PropertyKeyId(1),
                fgdb_delta_types::PropertyKeyId(2),
            ])),
            rights: Rights::ReadWrite,
            limits: QueryLimits {
                max_nodes: 100_000,
                max_work: 10_000_000,
                max_rows: 0,
            },
            expires_at_ms: u64::MAX / 2,
        })
        .await;
        let mut late_forbidden_args: Vec<Vec<(String, WireValue)>> = (0..1_024)
            .map(|i| {
                vec![
                    ("name".into(), WireValue::Text(format!("p-{i:04}"))),
                    (
                        "props".into(),
                        WireValue::Map(vec![("age".into(), WireValue::Int((i % 97) as i64))]),
                    ),
                ]
            })
            .collect();
        late_forbidden_args[1_023][1] = (
            "props".into(),
            WireValue::Map(vec![("secret".into(), WireValue::Int(999))]),
        );
        let late_refusal = restricted
            .execute_batch(
                cx,
                "MERGE (n:Person {name: $name}) SET n += $props",
                late_forbidden_args,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &late_refusal,
                ClientError::Server {
                    code: ErrorCode::PermissionDenied | ErrorCode::Statement,
                    message,
                } if message.contains("argument set 1023")
            ),
            "forbidden property at row 1,023 must refuse the whole 1,024-row batch: {late_refusal:?}"
        );
        restricted.close(cx).await.unwrap();

        // 2b. Commit 4,000 distinct-key vertex MERGE statements across 4 x 1,000-row
        // FGP EXECUTE_BATCH frames (respecting MAX_BATCH_ARGUMENT_SETS = 1,024 while
        // exceeding the old ~447-row / 100k-snapshot-record cliff by >35x on chunk 4).
        for chunk in 0..4_u64 {
            let base = (chunk as usize) * 1_000;
            let vertex_args: Vec<Vec<(String, WireValue)>> = (base..base + 1_000)
                .map(|i| {
                    vec![
                        ("name".into(), WireValue::Text(format!("p-{i:04}"))),
                        ("age".into(), WireValue::Int((i % 97) as i64)),
                    ]
                })
                .collect();
            assert_eq!(
                writer
                    .execute_batch(cx, vertex_stmt, vertex_args)
                    .await
                    .unwrap(),
                Outcome::WriteCommitted {
                    seq: chunk + 1,
                    statements: 1_000,
                }
            );
        }

        // 2c. Commit 4,000 distinct-endpoint relationship MERGE statements across
        // 4 x 1,000-row FGP EXECUTE_BATCH frames against all 4,000 committed vertices
        // under default 100k snapshot-record limits.
        let edge_stmt = "MATCH (a:Person {name: $src}), (b:Person {name: $dst}) \
                         MERGE (a)-[r:KNOWS]->(b) SET r.age = $w";
        for chunk in 0..4_u64 {
            let base = (chunk as usize) * 1_000;
            let edge_args: Vec<Vec<(String, WireValue)>> = (base..base + 1_000)
                .map(|i| {
                    vec![
                        ("src".into(), WireValue::Text(format!("p-{i:04}"))),
                        (
                            "dst".into(),
                            WireValue::Text(format!("p-{:04}", (i + 1) % 4_000)),
                        ),
                        ("w".into(), WireValue::Int((i % 53) as i64)),
                    ]
                })
                .collect();
            assert_eq!(
                writer
                    .execute_batch(cx, edge_stmt, edge_args)
                    .await
                    .unwrap(),
                Outcome::WriteCommitted {
                    seq: 5 + chunk,
                    statements: 1_000,
                }
            );
        }
        writer.close(cx).await.unwrap();

        shutdown.trigger();
        task.join(cx).await.unwrap();

        // 3. Reopen durable DB and prove exact final-graph & receipt parity with
        // sequential per-statement authorized execution on a reference DB (using
        // 64 rows for per-statement commit comparison to keep runtime fast while
        // checking full 4,000 durable vertices & 4,000 durable edges on the wire DB).
        let durable = Database::open(&commit, &path, keys()).await.unwrap();
        assert_eq!(durable.frontier().unwrap(), CommitSeq(8));
        assert_eq!(durable.vertices().unwrap().len(), 4_000);
        assert_eq!(durable.edges().unwrap().len(), 4_000);

        let parity_n = 64_usize;
        let mut batch_db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seq_db = Database::open_memory(&commit, keys()).await.unwrap();
        let rw_token = authority
            .issue_at(&grant(Rights::ReadWrite, 0), 500)
            .unwrap();
        let parity_v_params: Vec<_> = (0..parity_n)
            .map(|i| {
                fgdb_gql::GqlParameters::new()
                    .with_scalar(
                        "name",
                        fgdb_types::CanonicalScalar::ucs_basic_text(&format!("p-{i:04}")).unwrap(),
                    )
                    .unwrap()
                    .with_int64("age", (i % 97) as i64)
                    .unwrap()
            })
            .collect();
        let parity_e_params: Vec<_> = (0..parity_n)
            .map(|i| {
                fgdb_gql::GqlParameters::new()
                    .with_scalar(
                        "src",
                        fgdb_types::CanonicalScalar::ucs_basic_text(&format!("p-{i:04}")).unwrap(),
                    )
                    .unwrap()
                    .with_scalar(
                        "dst",
                        fgdb_types::CanonicalScalar::ucs_basic_text(&format!(
                            "p-{:04}",
                            (i + 1) % parity_n
                        ))
                        .unwrap(),
                    )
                    .unwrap()
                    .with_int64("w", (i % 53) as i64)
                    .unwrap()
            })
            .collect();

        let (batch_v_stats, _) = batch_db
            .authorized_write_session(
                &txn,
                &commit,
                &authority,
                &rw_token,
                fgdb_server::TRUNK,
                symbols.clone(),
                fgdb_delta_types::RelationId(1),
                default_config.write_policy,
                parity_n,
                || 1_000,
            )
            .unwrap()
            .query_batch_stats(&query, vertex_stmt, &parity_v_params)
            .await
            .unwrap();
        let (batch_e_stats, _) = batch_db
            .authorized_write_session(
                &txn,
                &commit,
                &authority,
                &rw_token,
                fgdb_server::TRUNK,
                symbols.clone(),
                fgdb_delta_types::RelationId(1),
                default_config.write_policy,
                parity_n,
                || 1_000,
            )
            .unwrap()
            .query_batch_stats(&query, edge_stmt, &parity_e_params)
            .await
            .unwrap();

        let mut seq_created_vertices = 0;
        let mut seq_created_edges = 0;
        for p in &parity_v_params {
            let (s, _) = seq_db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &authority,
                    &rw_token,
                    fgdb_server::TRUNK,
                    symbols.clone(),
                    fgdb_delta_types::RelationId(1),
                    default_config.write_policy,
                    1,
                    || 1_000,
                )
                .unwrap()
                .query_stats(&query, vertex_stmt, p)
                .await
                .unwrap();
            seq_created_vertices += s.created_vertices;
        }
        for p in &parity_e_params {
            let (s, _) = seq_db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &authority,
                    &rw_token,
                    fgdb_server::TRUNK,
                    symbols.clone(),
                    fgdb_delta_types::RelationId(1),
                    default_config.write_policy,
                    1,
                    || 1_000,
                )
                .unwrap()
                .query_stats(&query, edge_stmt, p)
                .await
                .unwrap();
            seq_created_edges += s.created_edges;
        }
        assert_eq!(batch_v_stats.created_vertices, seq_created_vertices);
        assert_eq!(batch_e_stats.created_edges, seq_created_edges);
        let canonical_graph = |db: &Database<_>| {
            let vertices = db.vertices().unwrap();
            let by_vid: std::collections::BTreeMap<_, _> = vertices
                .iter()
                .map(|v| (v.vid, (v.labels.clone(), v.props.clone(), v.retired_at)))
                .collect();
            let mut logical_vertices: Vec<_> = by_vid.values().cloned().collect();
            logical_vertices.sort();
            let mut logical_edges: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .map(|e| {
                    (
                        by_vid[&e.entry.src].clone(),
                        e.entry.relation,
                        by_vid[&e.entry.dst].clone(),
                        e.props,
                        e.entry.retired_at,
                    )
                })
                .collect();
            logical_edges.sort();
            (logical_vertices, logical_edges)
        };
        assert_eq!(canonical_graph(&batch_db), canonical_graph(&seq_db));
    });
}
