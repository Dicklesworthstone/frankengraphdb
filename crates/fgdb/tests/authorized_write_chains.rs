//! Real capabilities, native chain collectors, transaction staging and reopen.
//! Private WITH rows cannot bypass scope, quotas or the one commit boundary.

#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertQueryText, PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Restriction, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x60; 32], NS, [0x62; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9167), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rights: Rights, max_rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows,
        },
        expires_at_ms: 10_000,
    }
}

fn policy(rows: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000),
        100,
        100,
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn chain(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    let template = PreparedGraphWriteChainText::prepare(text, R, symbols).expect(text);
    let result = template.bind_parameters(&GqlParameters::new()).expect(text);
    assert_eq!(template.returns_rows(), result.returns_rows());
    result
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}

fn csv_chain(text: &str) -> PreparedGraphWriteChain {
    use fgdb_gql::csv_parameters::CsvParameterLimits;
    use fgdb_gql::{GqlParameterSpec, GqlParameterType};
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        text,
        R,
        &[("rows", GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    let schema: Vec<_> = ["p", "q"]
        .into_iter()
        .map(|name| GqlParameterSpec {
            name: name.into(),
            parameter_type: GqlParameterType::Int64,
            requires_positive: false,
            occurrences: 1,
        })
        .collect();
    let mut input = b"p,q\n2,1\n1,2\n".as_slice();
    let (chain, records) = template
        .bind_csv_rows_stream_controlled(
            "rows",
            &schema,
            CsvParameterLimits::default(),
            |buffer| -> Result<usize, ()> {
                let count = buffer.len().min(3).min(input.len());
                buffer[..count].copy_from_slice(&input[..count]);
                input = &input[count..];
                Ok(count)
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(records, 2);
    chain
}

#[test]
fn csv_initial_relations_match_list_oracle_through_native_and_authorized_hosts() {
    lab(0xc4a1_20c1, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let arguments = GqlParameters::new()
            .with_list(
                "rows",
                [(2, 1), (1, 2)]
                    .into_iter()
                    .map(|(p, q)| {
                        GraphValue::map(vec![
                            ("p".into(), GraphValue::Scalar(CanonicalScalar::Int(p))),
                            ("q".into(), GraphValue::Scalar(CanonicalScalar::Int(q))),
                        ])
                        .unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        for text in [
            "UNWIND $rows AS row MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row WITH row MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row UNWIND [2,1] AS repeat MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row MATCH (seed:Visible {p:row.p}) MERGE (a:Visible {p:row.q}) MERGE (seed)-[:R]->(a)",
            "UNWIND $rows AS row CREATE (a:Visible {p:row.p}) WITH row,a CREATE (b:Visible {p:row.q}), (a)-[:R]->(b)",
            "UNWIND $rows AS row MATCH (a:Visible {p:row.p}), (b:Visible {p:row.q}) MERGE (a)-[:R]->(b) MERGE (a)-[:S]->(b)",
        ] {
            assert!(
                PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
            let ordinary = PreparedGraphWriteChainText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&arguments)
                .unwrap();
            let streamed = csv_chain(text);
            let mut expected = None;
            for mode in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(10), vec![L], vec![(P, CanonicalScalar::Int(1))]);
                seed.create_vertex(VId(20), vec![L], vec![(P, CanonicalScalar::Int(2))]);
                db.write(&commit, seed).await.unwrap();
                let basis = db.frontier().unwrap();
                if mode == 2 {
                    db.execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &streamed,
                        policy(10),
                        || NOW,
                    )
                    .await
                    .unwrap();
                } else {
                    let mut transaction = db.begin(&txn).unwrap();
                    transaction
                        .execute_graph_write_chain_engine_governed(
                            &mut db,
                            &query,
                            if mode == 0 { &ordinary } else { &streamed },
                            policy(10),
                        )
                        .unwrap();
                    transaction.finish(&mut db, &commit).await.unwrap();
                }
                assert_eq!(db.frontier().unwrap().0, basis.0 + 1, "mode {mode}: {text}");
                let actual = (db.vertices().unwrap(), db.edges().unwrap());
                if let Some(expected) = &expected {
                    assert_eq!(&actual, expected, "mode {mode}: {text}");
                } else {
                    expected = Some(actual);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn csv_input_admission_and_late_clause_failure_publish_no_prefix() {
    lab(0xc4a1_20c2, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let program = csv_chain(
            "UNWIND $rows AS row MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q+10}) MERGE (a)-[:R]->(b)",
        );
        for authorized in [false, true] {
            for input_limit in [true, false] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                let mut allowance = policy(10);
                if input_limit {
                    allowance.query.evaluator.max_scratch_entries = 0;
                } else {
                    allowance.max_vertices = 3;
                }
                if authorized {
                    assert!(
                        db.execute_graph_write_chain_authorized(
                            &txn,
                            &query,
                            &commit,
                            &issuer,
                            &token,
                            "main",
                            &program,
                            allowance,
                            || NOW
                        )
                        .await
                        .is_err()
                    );
                } else {
                    let mut transaction = db.begin(&txn).unwrap();
                    assert!(
                        transaction
                            .execute_graph_write_chain_engine_governed(
                                &mut db, &query, &program, allowance
                            )
                            .is_err()
                    );
                    transaction.abort();
                }
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let forbidden =
            csv_chain("UNWIND $rows AS row MERGE (a:Hidden {p:row.p}) MERGE (b:Visible {p:row.q})");
        let mut no_work = policy(10);
        no_work.query.evaluator.max_work_units = 0;
        let error = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &forbidden,
                no_work,
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(
            authorization(&error),
            Some(Error::ScopeDenied),
            "authorization must precede input copying: {error:?}"
        );
        assert!(db.vertices().unwrap().is_empty());
    });
}

#[test]
fn write_only_private_rows_create_multiple_relations_in_one_reopenable_commit() {
    lab(0xc4a1_2001, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let program = chain(
            "CREATE (a:Visible {p:3}) WITH a CREATE (b:Visible {p:a.p+4}), \
             (a)-[:R]->(b), (b)-[:S]->(a)",
        );
        let (stats, result, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        assert_eq!(
            (result.rows.snapshot_records, result.rows.result_rows),
            (0, 0)
        );
        assert!(result.value.is_empty());
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, basis.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        let mut values: Vec<_> = vertices.iter().map(|row| row.props[0].1.clone()).collect();
        values.sort();
        assert_eq!(
            values,
            vec![
                CanonicalScalar::Int(3).into(),
                CanonicalScalar::Int(7).into()
            ]
        );
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].entry.src, edges[1].entry.dst);
        assert_eq!(edges[0].entry.dst, edges[1].entry.src);
        assert_eq!(txn.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
    });
}

#[test]
fn final_rows_alone_spend_signed_rows_and_paging_keeps_all_creations() {
    lab(0xc4a1_2002, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        for rows in [0, 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let program = chain(&format!(
                "UNWIND [1,2] AS x CREATE (a:Visible {{p:x}}) WITH a \
                 CREATE (b:Visible {{p:a.p+10}}), (a)-[:R]->(b) \
                 RETURN a.p AS before, b.p AS after ORDER BY after DESC LIMIT {rows}"
            ));
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &limited,
                    "main",
                    &program,
                    policy(rows),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
            assert_eq!(result.value.len() as u64, rows);
            assert_eq!(result.rows.result_rows, rows);
            if rows == 1 {
                assert_eq!(
                    result.value[0].values(),
                    &[
                        GraphValue::Scalar(CanonicalScalar::Int(2)),
                        GraphValue::Scalar(CanonicalScalar::Int(12)),
                    ]
                );
            }
            assert_eq!(
                (db.vertices().unwrap().len(), db.edges().unwrap().len()),
                (4, 2)
            );
        }
    });
}

#[test]
fn hidden_carried_fields_refuse_and_selected_inputs_remain_masked() {
    lab(0xc4a1_2003, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(40))]);
        db.write(&commit, seed).await.unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        for text in [
            "CREATE (a:Visible {p:1}) WITH a CREATE (b:Visible {p:a.secret})",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b CREATE (c:Visible {p:a.secret})",
        ] {
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ScopeDenied), "{error:?}");
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        }
        let program = chain(
            "MATCH (a:Visible) CREATE (b:Visible {p:a.p}) WITH a,b \
             CREATE (b)-[:R]->(a) RETURN b.p AS p",
        );
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(10),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (1, 1));
        assert_eq!(
            result.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(7))]
        );
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap(), before.1[1]);
    });
}

#[test]
fn a_late_scope_expression_quota_or_output_refusal_discards_every_step() {
    lab(0xc4a1_2004, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let cases = [
            ("CREATE (a:Visible) WITH a CREATE (b:Hidden)", 100),
            ("CREATE (a:Visible) WITH a CREATE (b:Visible {p:1/0})", 100),
            ("CREATE (a:Visible) WITH a CREATE (b:Visible)", 1),
        ];
        for (text, vertices) in cases {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let mut bounded = policy(10);
            bounded.max_vertices = vertices;
            let program = chain(text);
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "{text}"
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = db.frontier().unwrap();
        let program = chain("CREATE (a:Visible) WITH a CREATE (b:Visible) RETURN b");
        let mut called = 0;
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
                |rows, meter| {
                    called += 1;
                    assert_eq!(rows.len(), 1);
                    meter(1)?;
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(called, 1);
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn source_free_with_computation_spends_the_same_signed_work_permit() {
    lab(0xc4a1_2005, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(Rights::Write, 0), NOW)
            .unwrap()
            .attenuate(Restriction::MaxWork(512))
            .unwrap();
        for (value, succeeds) in [("512", true), ("size(range(1,512))", false)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let program = chain(&format!(
                "CREATE (a:Visible) WITH {value} AS n CREATE (b:Visible {{p:n}})"
            ));
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0),
                    || NOW,
                )
                .await;
            assert_eq!(result.is_ok(), succeeds, "{value}: {result:?}");
            if !succeeds {
                assert!(matches!(
                    authorization(&result.unwrap_err()),
                    Some(Error::LimitExceeded(_))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertices().unwrap().is_empty());
            }
        }
    });
}

#[test]
fn write_only_merges_mutations_and_public_reads_fail_before_publication() {
    lab(0xc4a1_2006, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 10), NOW).unwrap();
        for text in [
            "CREATE (a:Visible) WITH a CREATE (b:Visible) RETURN b",
            "MATCH (a:Visible) CREATE (b:Visible) WITH b CREATE (c:Visible)",
            "CREATE (a:Visible) SET a.p = 3",
            "CREATE (a:Visible) MERGE (b:Visible {p:3})",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::PermissionDenied),
                "{error:?}"
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn delete_projection_and_incidence_proof_share_each_native_allowance() {
    lab(0xc4a1_2014, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let complete = Grant {
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::All,
            ..grant(Rights::ReadWrite, 0)
        };
        let token = issuer.issue_at(&complete, NOW).unwrap();
        let program = chain(
            "MATCH (a:Visible {p:7}) CREATE (audit:Visible {p:99}) WITH a,audit \
             DELETE a RETURN size(range(1,32)) AS size LIMIT 0",
        );
        let mut measured = None;
        for attempt in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(7))]);
            seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(8))]);
            seed.create_vertex(VId(3), vec![L], vec![(P, CanonicalScalar::Int(9))]);
            // This unrelated visible edge must be inspected by the isolation
            // proof after the RETURN projection has already consumed its work.
            seed.add_edge(EId(1), VId(2), VId(3), vec![]);
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut bounded = policy(0).with_mutation_limit(1);
            if let Some((records, work, scratch)) = measured {
                bounded.query = GqlQueryPolicy::new(
                    records - u64::from(attempt == 4),
                    0,
                    work - u64::from(attempt == 2),
                    scratch - u64::from(attempt == 3),
                );
            }
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if attempt < 2 {
                let (_, result, _) = result.unwrap();
                let usage = (
                    result.rows.snapshot_records,
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                );
                if let Some(expected) = measured {
                    assert_eq!(
                        usage, expected,
                        "exact cumulative limits must still succeed"
                    );
                } else {
                    assert!(usage.0 > 0 && usage.1 > 1 && usage.2 > 0);
                    measured = Some(usage);
                }
                assert!(result.value.is_empty(), "LIMIT 0 changes no effect");
                assert!(db.vertex(VId(1)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            } else {
                let error = result.unwrap_err();
                if attempt == 4 {
                    assert!(
                        matches!(error, fgdb_gql::GqlQueryError::Rows(_)),
                        "{error:?}"
                    );
                } else {
                    assert!(
                        matches!(error, fgdb_gql::GqlQueryError::Evaluator(_)),
                        "{error:?}"
                    );
                }
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before,
                    "neither the creation nor deletion may publish after a combined-limit refusal",
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn created_vertex_and_edge_mutations_return_post_statement_values_and_reopen() {
    lab(0xc4a1_2011, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        for (text, expected, vertices, edges) in [
            (
                "CREATE (a:Visible {p:3}) SET a.p=a.p+4 RETURN a.p AS p",
                CanonicalScalar::Int(7),
                1,
                0,
            ),
            (
                "CREATE (a:Visible {p:5}) REMOVE a.p RETURN a.p AS p",
                CanonicalScalar::Null,
                1,
                0,
            ),
            (
                "CREATE (a:Visible), (b:Visible), (a)-[r:R {p:2}]->(b) \
                 SET r.p=r.p+6 RETURN r.p AS p",
                CanonicalScalar::Int(8),
                2,
                1,
            ),
        ] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = db.frontier().unwrap();
            let program = chain(text);
            let (stats, result, completion) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(1),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                (vertices, edges)
            );
            assert_eq!(result.value.len(), 1);
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(expected.clone())]
            );
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier
                }
            );
            let stored_vertices = db.vertices().unwrap();
            let stored_edges = db.edges().unwrap();
            let fields = if edges == 0 {
                &stored_vertices[0].props
            } else {
                &stored_edges[0].props
            };
            if expected == CanonicalScalar::Null {
                assert!(fields.is_empty(), "REMOVE must unset, not store NULL");
            } else {
                assert_eq!(fields, &vec![(P, expected.into())]);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), stored_vertices);
            assert_eq!(reopened.edges().unwrap(), stored_edges);
        }
    });
}

#[test]
fn selected_mutation_chains_preserve_hidden_fields_and_abort_every_late_refusal() {
    lab(0xc4a1_2012, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(40))]);
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for text in [
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b SET a.secret=99",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b SET a.p=a.secret",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b SET a.p=8 RETURN a.secret",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b DETACH DELETE a",
        ] {
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::ScopeDenied),
                "{text}: {error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let program = chain(
            "MATCH (a:Visible) CREATE (b:Visible {p:a.p}) WITH a,b \
             SET a.p=a.p+1 RETURN a.p AS updated, b.p AS previous",
        );
        let error = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1).with_mutation_limit(0),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mutation effect limit exceeded"));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before,
        );
        let mut calls = 0;
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1).with_mutation_limit(1),
                || NOW,
                |rows, meter| {
                    calls += 1;
                    assert_eq!(
                        rows[0].values(),
                        &[
                            GraphValue::Scalar(CanonicalScalar::Int(8)),
                            GraphValue::Scalar(CanonicalScalar::Int(7)),
                        ]
                    );
                    meter(1)?;
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before,
        );
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &token,
            "main",
            &program,
            policy(1).with_mutation_limit(1),
            || NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(8).into()),
                (SECRET, CanonicalScalar::Int(99).into()),
            ]
        );
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap(), before.1[1]);
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn plain_and_detach_delete_chains_keep_native_incidence_rules_and_reopen() {
    lab(0xc4a1_2013, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let complete = Grant {
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::All,
            ..grant(Rights::ReadWrite, 1)
        };
        let token = issuer.issue_at(&complete, NOW).unwrap();
        for detaching in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(7))]);
            seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(8))]);
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let isolated = chain(
                "MATCH (a:Visible {p:7}) CREATE (audit:Visible {p:99}) WITH a,audit \
                 DELETE a RETURN audit.p AS p",
            );
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &isolated,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("incident relationship"),
                "{error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            let (text, effects) = if detaching {
                (
                    "MATCH (a:Visible {p:7}) CREATE (audit:Visible {p:99}) WITH a,audit \
                     DETACH DELETE a RETURN audit.p AS p",
                    1,
                )
            } else {
                (
                    "MATCH (a:Visible {p:7})-[r:R]->(b:Visible) \
                     CREATE (audit:Visible {p:99}) WITH a,r,audit \
                     DELETE a,r RETURN audit.p AS p",
                    2,
                )
            };
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(effects - 1),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("mutation effect limit exceeded"),
                "{error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(effects),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(99))]
            );
            assert!(db.vertex(VId(1)).unwrap().is_none());
            assert!(db.vertex(VId(2)).unwrap().is_some());
            assert!(db.edges().unwrap().is_empty());
            let survivors = db.vertices().unwrap();
            assert_eq!(survivors.len(), 2);
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0.0 + 1);
            assert_eq!(txn.outstanding_obligations(), 0);
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), survivors);
            assert!(reopened.edges().unwrap().is_empty());
        }
    });
}

#[test]
fn computed_create_output_and_private_with_read_the_admitted_staged_bag() {
    lab(0xc4a1_2014, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let definition = PreparedGraphInsertQueryText::prepare(
            "UNWIND [2,3] AS x CREATE (a:Visible {p:x})-[r:R {p:x*10}]->(b:Visible {p:x+1}) \
             WITH collect(a) AS ns, sum([e IN [r] | e.p][0]) AS total \
             RETURN [n IN ns | n.p] AS values, total",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let (stats, rows, _) = db
            .execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &definition,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::List(
                    vec![
                        GraphValue::Scalar(CanonicalScalar::Int(2)),
                        GraphValue::Scalar(CanonicalScalar::Int(3))
                    ]
                    .into_boxed_slice()
                ),
                GraphValue::Scalar(CanonicalScalar::Int(50)),
            ]
        );
        let program = chain(
            "CREATE (a:Visible {p:7}) WITH a, [v IN [a] | v.p][0] AS old \
             CREATE (b:Visible {p:old+1}) RETURN [v IN [a,b] | v.p] AS values",
        );
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::Int(7)),
                    GraphValue::Scalar(CanonicalScalar::Int(8))
                ]
                .into_boxed_slice()
            ),]
        );
        assert_eq!(db.vertices().unwrap().len(), 6);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_merge_continuations_match_native_bags_and_rollback_late_failures() {
    lab(0xc4a1_2017, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let mut scope = grant(Rights::ReadWrite, 3);
        scope.properties = Scope::only([P, SECRET]);
        let token = issuer.issue_at(&scope, NOW).unwrap();
        for (prefix, returned, edge_count) in [
            (
                "UNWIND [1,2,3] AS i MERGE (a:Visible {secret:1}) \
                 ON CREATE SET a.p=0 ON MATCH SET a.p=a.p+1 \
                 WITH a,[v IN [a] | v.p] AS prior \
                 MERGE (b:Visible {secret:2}) ON CREATE SET b.p=7",
                "b",
                0,
            ),
            (
                "CREATE (a:Visible {p:1}),(b:Visible {p:2}) \
                 WITH a,b UNWIND [1,2,3] AS i \
                 MERGE (a)-[r:R]->(b) ON CREATE SET r.p=0 ON MATCH SET r.p=r.p+1 \
                 WITH a,b,r,[e IN [r] | e.p] AS prior \
                 MERGE (a)-[s:S {p:7}]->(b)",
                "s",
                2,
            ),
            (
                "CREATE (a:Visible {p:1}),(b:Visible {p:2}) \
                 WITH a,b UNWIND [1,2,3] AS i \
                 MERGE (a)-[r:R]->(b) ON CREATE SET r.p=0 ON MATCH SET r.p=r.p+1 \
                 WITH a,b,r,[e IN [r] | e.p] AS prior \
                 MERGE (a)-[s:S]->(b) ON CREATE SET s.p=7",
                "s",
                2,
            ),
        ] {
            for fail_output in [false, true] {
                let expression = if fail_output { "1/(v.p-7)" } else { "v.p" };
                let text = format!(
                    "{prefix} RETURN prior,[v IN [{returned}] | {expression}] AS next {}",
                    if fail_output { "LIMIT 0" } else { "" },
                );
                let program = chain(&text);
                for authorized in [false, true] {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    let frontier = db.frontier().unwrap();
                    let output = if authorized {
                        let result = db
                            .execute_graph_write_chain_authorized(
                                &txn,
                                &query,
                                &commit,
                                &issuer,
                                &token,
                                "main",
                                &program,
                                policy(if fail_output { 0 } else { 3 }),
                                || NOW,
                            )
                            .await;
                        if fail_output {
                            assert!(result.is_err(), "authorized: {text}");
                            None
                        } else {
                            Some(result.unwrap().1)
                        }
                    } else {
                        let mut transaction = db.begin(&txn).unwrap();
                        let result = transaction.execute_graph_write_chain_engine_governed(
                            &mut db,
                            &query,
                            &program,
                            policy(if fail_output { 0 } else { 3 }),
                        );
                        if fail_output {
                            assert!(result.is_err(), "native: {text}");
                            transaction.abort();
                            None
                        } else {
                            let output = result.unwrap().1;
                            transaction.finish(&mut db, &commit).await.unwrap();
                            Some(output)
                        }
                    };
                    if let Some(output) = output {
                        assert_eq!(output.value.len(), 3, "{text}");
                        for row in output.value {
                            assert_eq!(
                                row.values(),
                                &[
                                    GraphValue::List(
                                        vec![GraphValue::Scalar(CanonicalScalar::Int(2))]
                                            .into_boxed_slice(),
                                    ),
                                    GraphValue::List(
                                        vec![GraphValue::Scalar(CanonicalScalar::Int(7))]
                                            .into_boxed_slice(),
                                    ),
                                ],
                                "authorized={authorized}: {text}"
                            );
                        }
                        assert_eq!(db.vertices().unwrap().len(), 2);
                        assert_eq!(db.edges().unwrap().len(), edge_count);
                        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
                    } else {
                        assert!(db.vertices().unwrap().is_empty());
                        assert!(db.edges().unwrap().is_empty());
                        assert_eq!(db.frontier().unwrap(), frontier);
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
            }
        }
    });
}

#[test]
fn computed_create_output_masks_untrusted_identities_and_refuses_hidden_fields() {
    lab(0xc4a1_2015, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(11))]);
        seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(22))]);
        seed.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(33))]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(44))]);
        seed.add_edge(EId(12), VId(1), VId(3), vec![(P, CanonicalScalar::Int(55))]);
        seed.add_edge(EId(13), VId(3), VId(2), vec![(P, CanonicalScalar::Int(66))]);
        db.write(&commit, seed).await.unwrap();
        let mut outside = WriteBatch::new(RelationId(3));
        outside.add_edge(EId(14), VId(1), VId(2), vec![(P, CanonicalScalar::Int(77))]);
        db.write(&commit, outside).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let definition = PreparedGraphInsertQueryText::prepare_with_parameter_types(
            "CREATE (n:Visible {p:7}) RETURN [v IN $refs | v.p] AS values",
            R,
            &[("refs", GqlParameterType::List)],
            symbols,
        )
        .unwrap()
        .bind_parameters(
            &GqlParameters::new()
                .with_list(
                    "refs",
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Edge(EId(11)),
                        GraphValue::Vertex(VId(3)),
                        GraphValue::Vertex(VId(999)),
                        GraphValue::Edge(EId(12)),
                        GraphValue::Edge(EId(13)),
                        GraphValue::Edge(EId(14)),
                        GraphValue::Edge(EId(999)),
                    ],
                )
                .unwrap(),
        )
        .unwrap();
        let (_, rows, _) = db
            .execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &definition,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        let mut expected = vec![
            GraphValue::Scalar(CanonicalScalar::Int(11)),
            GraphValue::Scalar(CanonicalScalar::Int(44)),
        ];
        expected.extend((0..6).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::List(expected.into_boxed_slice())]
        );
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for text in [
            "CREATE (n:Visible {p:7}) RETURN [v IN [n] | v.secret][0] AS hidden",
            "UNWIND [1,0] AS x CREATE (n:Visible {p:x}) RETURN [v IN [n] | 1/v.p][0] AS value LIMIT 0",
        ] {
            let definition = PreparedGraphInsertQueryText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let error = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &definition,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap_err();
            if text.contains("secret") {
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            }
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn blind_create_continuations_cannot_read_parameter_carried_element_ids() {
    lab(0xc4a1_2016, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(11))]);
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(22))]);
        db.write(&commit, seed).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
            "CREATE (a:Visible) WITH [v IN $refs | v.p][0] AS copied \
             CREATE (b:Visible {p:copied})",
            R,
            &[("refs", GqlParameterType::List)],
            symbols,
        )
        .unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        for identity in [
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(2)),
            GraphValue::Vertex(VId(999)),
            GraphValue::Edge(EId(999)),
        ] {
            let program = template
                .bind_parameters(
                    &GqlParameters::new()
                        .with_list("refs", vec![identity])
                        .unwrap(),
                )
                .unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::PermissionDenied),
                "{error:?}"
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        }
        // Returning a known CREATE binding to a private continuation retains
        // the existing blind-write contract; it performs no element lookup.
        let own = chain("CREATE (a:Visible {p:3}) WITH a, a.p AS p CREATE (b:Visible {p:p+1})");
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &own,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert!(rows.value.is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

fn post_write_pattern(key: PropertyKeyId) -> fgdb_gql::PreparedGraphSet {
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison, VertexPredicate};
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("matched").unwrap();
    graph
        .filter(
            "matched",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Equal,
                value: 1,
            },
        )
        .unwrap();
    graph
        .prepare_values(
            &[
                GraphColumn::vertex("matched", "matched"),
                GraphColumn::property("matched_p", "matched", key),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into()
}

fn post_write_read(
    chain: &PreparedGraphWriteChain,
    key: PropertyKeyId,
) -> fgdb_gql::PreparedGraphSet {
    use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
    let left = chain.read_input().unwrap();
    let right = post_write_pattern(key);
    let join = RowJoinSpec::new(left.column_types(), right.column_types(), &[(0, 0)])
        .unwrap()
        .with_kind(RowJoinKind::Left);
    left.join(right, join).unwrap()
}

#[test]
fn authorized_post_write_read_sees_staged_bags_masks_hidden_rows_and_reopens() {
    lab(0xc4a1_20d1, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        let continuation = post_write_read(&program, P);
        let mut expected_usage = None;
        for hidden in [0, 32] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            if hidden != 0 {
                let mut seed = WriteBatch::new(R);
                for id in 1..=hidden {
                    seed.create_vertex(VId(id), vec![HIDDEN], vec![(P, CanonicalScalar::Int(1))]);
                }
                db.write(&commit, seed).await.unwrap();
            }
            let before = db.frontier().unwrap();
            let (stats, result, completion) = db
                .execute_graph_write_chain_then_read_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    policy(3),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (2, 0));
            assert_eq!(stats.selection, result.rows);
            assert_eq!(stats.evaluator, result.evaluator);
            assert_eq!(result.rows.result_rows, 3);
            let mut observed = Vec::new();
            for row in &result.value {
                let [GraphValue::Vertex(id), matched, value] = row.values() else {
                    panic!("unexpected continuation row: {row:?}")
                };
                let vertex = db.vertex(*id).unwrap().unwrap();
                let p = vertex
                    .props
                    .iter()
                    .find(|(key, _)| *key == P)
                    .unwrap()
                    .1
                    .clone();
                if p == CanonicalScalar::Int(1).into() {
                    assert_eq!(matched, &GraphValue::Vertex(*id));
                    assert_eq!(value, &GraphValue::Scalar(CanonicalScalar::Int(1)));
                    observed.push(1);
                } else {
                    assert_eq!(p, CanonicalScalar::Int(2).into());
                    assert_eq!(matched, &GraphValue::Scalar(CanonicalScalar::Null));
                    assert_eq!(value, &GraphValue::Scalar(CanonicalScalar::Null));
                    observed.push(2);
                }
            }
            observed.sort_unstable();
            assert_eq!(observed, [1, 1, 2]);
            if let Some(expected) = expected_usage {
                assert_eq!(stats, expected, "hidden rows cannot affect public usage");
            } else {
                expected_usage = Some(stats);
            }
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier
                }
            );
            let expected = db.vertices().unwrap();
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), expected);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_post_write_read_charges_only_final_rows_and_admits_them_before_commit() {
    use fgdb_gql::{GraphSetOperand, GraphSetPredicateOp};
    lab(0xc4a1_20d2, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        let continuation = post_write_read(&program, P)
            .filter(&[GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            }])
            .unwrap();
        for refuse_output in [true, false] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut called = 0;
            let result = db
                .execute_graph_write_chain_then_read_authorized_checked(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    policy(1),
                    || NOW,
                    |rows, meter| {
                        called += 1;
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].values().len(), 3);
                        meter(1)?;
                        if refuse_output {
                            Err(WriteTxnError::Authorization(Error::ScopeDenied))
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            assert_eq!(called, 1);
            if refuse_output {
                assert_eq!(
                    authorization(&result.unwrap_err()),
                    Some(Error::ScopeDenied)
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
            } else {
                let (stats, result, _) = result.unwrap();
                assert_eq!(stats.created_vertices, 2);
                assert_eq!(result.rows.result_rows, 1);
                assert_eq!(db.vertices().unwrap().len(), 2);
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_post_write_read_schema_rights_scope_and_row_failures_publish_nothing() {
    lab(0xc4a1_20d3, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        for case in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let rights = if case == 1 {
                Rights::Write
            } else {
                Rights::ReadWrite
            };
            let token = issuer
                .issue_at(&grant(rights, if case == 3 { 2 } else { 3 }), NOW)
                .unwrap();
            let continuation = if case == 0 {
                post_write_pattern(P)
            } else {
                post_write_read(&program, if case == 4 { SECRET } else { P })
            };
            let error = db
                .execute_graph_write_chain_then_read_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    policy(if case == 2 { 2 } else { 3 }),
                    || NOW,
                )
                .await
                .unwrap_err();
            if case == 1 {
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            } else if case == 4 {
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            }
            assert_eq!(db.frontier().unwrap(), before, "case {case}: {error:?}");
            assert!(db.vertices().unwrap().is_empty(), "case {case}");
            assert!(db.edges().unwrap().is_empty(), "case {case}");
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_post_write_read_shares_each_native_allowance_with_its_write_prefix() {
    lab(0xc4a1_20d4, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        let continuation = post_write_read(&program, P);
        let mut write_db = Database::open_memory(&commit, keys()).await.unwrap();
        let (prefix, _, _) = write_db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(3),
                || NOW,
            )
            .await
            .unwrap();
        let mut complete_db = Database::open_memory(&commit, keys()).await.unwrap();
        let (complete, _, _) = complete_db
            .execute_graph_write_chain_then_read_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                &continuation,
                policy(3),
                || NOW,
            )
            .await
            .unwrap();
        assert!(complete.selection.snapshot_records > prefix.selection.snapshot_records);
        assert!(complete.evaluator.work_units > prefix.evaluator.work_units);
        assert!(complete.evaluator.scratch_entries > prefix.evaluator.scratch_entries);
        for dimension in 0..3 {
            let mut bounded = policy(3);
            match dimension {
                0 => {
                    bounded.query = GqlQueryPolicy::new(
                        complete.selection.snapshot_records - 1,
                        3,
                        bounded.query.evaluator.max_work_units,
                        bounded.query.evaluator.max_scratch_entries,
                    );
                }
                1 => bounded.query.evaluator.max_work_units = complete.evaluator.work_units - 1,
                _ => {
                    bounded.query.evaluator.max_scratch_entries =
                        complete.evaluator.scratch_entries - 1
                }
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            assert!(
                db.execute_graph_write_chain_then_read_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "dimension {dimension}"
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
