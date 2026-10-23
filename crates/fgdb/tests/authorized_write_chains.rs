//! Real capabilities, native chain collectors, transaction staging and reopen.
//! Private WITH rows cannot bypass scope, quotas or the one commit boundary.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
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
