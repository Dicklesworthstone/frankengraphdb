//! Native relationship MERGE chains share scope, work and one atomic completion.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, TxnCx, VId,
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

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn whole_pair_reduction_preserves_parallel_orientation_and_private_bag_rows() {
    lab(0xc4a1_3101, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 5), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=4 {
            seed.create_vertex(
                VId(id as u128),
                vec![L],
                vec![(P, CanonicalScalar::Int(id))],
            );
        }
        // Pair order differs from edge order, and one input pair occurs twice.
        for (edge, destination) in [(11, 4), (12, 2), (13, 3), (14, 2)] {
            seed.add_edge(EId(edge), VId(1), VId(destination), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let forward = EId((1_u128 << 100) + 21);
        let reverse = EId((1_u128 << 100) + 22);
        let mut existing = WriteBatch::new(S);
        existing.add_edge(forward, VId(1), VId(3), vec![(P, CanonicalScalar::Int(7))]);
        existing.add_edge(reverse, VId(3), VId(1), vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, existing).await.unwrap();
        let before = db.frontier().unwrap();
        let program = chain(
            "MATCH (a:Visible)-[:R]->(b:Visible) CREATE (marker:Visible {p:b.p}) \
             MERGE (a)-[s:S {p:7}]-(b) \
             RETURN b.p AS destination, s AS edge, startNode(s) AS source, endNode(s) AS target \
             ORDER BY destination, edge",
        );
        let mut bounded = policy(5);
        bounded.max_edges = 2;
        let (stats, result, completion) = db
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
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
        assert_eq!(result.rows.result_rows, 5);
        let rows: Vec<_> = result.value.iter().map(GraphValueRow::values).collect();
        assert_eq!(
            rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
            vec![int(2), int(2), int(3), int(3), int(4)]
        );
        assert_eq!(rows[0][1], rows[1][1], "duplicate pairs share one creation");
        assert_eq!(
            rows[2],
            &[
                int(3),
                GraphValue::Edge(forward),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(3))
            ]
        );
        assert_eq!(
            rows[3],
            &[
                int(3),
                GraphValue::Edge(reverse),
                GraphValue::Vertex(VId(3)),
                GraphValue::Vertex(VId(1))
            ]
        );
        assert_eq!(
            db.edges()
                .unwrap()
                .iter()
                .filter(|edge| edge.entry.relation == S)
                .count(),
            4
        );
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );

        // Now MERGE is a private intermediate owner: each parallel match and
        // repeated source occurrence must reach the following CREATE.
        let program = chain(
            "MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[s:S {p:7}]-(b) \
             CREATE (copy:Visible {p:b.p}) RETURN copy.p AS destination ORDER BY destination",
        );
        bounded.max_edges = 0;
        let (stats, result, completion) = db
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
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (5, 0));
        assert_eq!(
            result
                .value
                .iter()
                .map(|row| row.values()[0].clone())
                .collect::<Vec<_>>(),
            vec![int(2), int(2), int(3), int(3), int(4)]
        );
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: db.frontier().unwrap(),
            }
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[allow(clippy::too_many_arguments)]
async fn hidden_attempt(
    commit: &CommitCx,
    txn: &TxnCx,
    query: &QueryCx,
    issuer: &Authority,
    hidden: bool,
    returned: bool,
    native_work: u64,
    signed_work: u64,
) -> Option<(GraphInsertStats, Vec<GraphValueRow>)> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=2 {
        let mut properties = vec![(P, CanonicalScalar::Int(id))];
        if hidden {
            properties.push((SECRET, CanonicalScalar::Int(99)));
        }
        seed.create_vertex(VId(id as u128), vec![L], properties);
    }
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    if hidden {
        seed.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(3))]);
        seed.create_vertex(VId(4), vec![HIDDEN], vec![(P, CanonicalScalar::Int(4))]);
        seed.add_edge(EId(12), VId(1), VId(3), vec![]);
        seed.add_edge(EId(13), VId(4), VId(2), vec![]);
    }
    db.write(commit, seed).await.unwrap();
    let mut existing = WriteBatch::new(S);
    let mut properties = vec![(P, CanonicalScalar::Int(7))];
    if hidden {
        properties.push((SECRET, CanonicalScalar::Int(88)));
        existing.add_edge(EId(22), VId(3), VId(2), vec![(P, CanonicalScalar::Int(7))]);
    }
    existing.add_edge(EId(21), VId(1), VId(2), properties);
    db.write(commit, existing).await.unwrap();
    let before = (
        db.frontier().unwrap(),
        db.vertices().unwrap(),
        db.edges().unwrap(),
    );
    let rows = u64::from(returned);
    let token = issuer
        .issue_at(&grant(Rights::ReadWrite, rows), NOW)
        .unwrap()
        .attenuate(Restriction::MaxWork(signed_work))
        .unwrap();
    let program = chain(if returned {
        "MATCH (a:Visible)-[:R]->(b:Visible) CREATE (marker:Visible {p:a.p}) \
         MERGE (a)-[s:S {p:7}]->(b) RETURN s.p AS relationship, a.p AS source, b.p AS target"
    } else {
        "MATCH (a:Visible)-[:R]->(b:Visible) CREATE (marker:Visible {p:a.p}) \
         MERGE (a)-[s:S {p:7}]->(b)"
    });
    let mut bounded = policy(rows);
    bounded.query.evaluator.max_work_units = native_work;
    bounded.max_edges = 0;
    let result = db
        .execute_graph_write_chain_authorized(
            txn,
            query,
            commit,
            issuer,
            &token,
            "main",
            &program,
            bounded,
            || NOW,
        )
        .await;
    assert_eq!(txn.outstanding_obligations(), 0);
    match result {
        Ok((stats, result, completion)) => {
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(result.rows.result_rows, rows);
            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: db.frontier().unwrap(),
                }
            );
            if returned {
                assert_eq!(result.value.len(), 1);
                assert_eq!(result.value[0].values(), &[int(7), int(1), int(2)]);
            } else {
                assert!(result.value.is_empty());
            }
            Some((stats, result.value))
        }
        Err(_) => {
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            None
        }
    }
}

#[test]
fn hidden_records_and_fields_do_not_change_merge_chain_work_or_admission() {
    lab(0xc4a1_3102, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for returned in [false, true] {
            let visible = hidden_attempt(
                &commit, &txn, &query, &issuer, false, returned, 1_000_000, 1_000_000,
            )
            .await
            .unwrap();
            let masked = hidden_attempt(
                &commit, &txn, &query, &issuer, true, returned, 1_000_000, 1_000_000,
            )
            .await
            .unwrap();
            assert_eq!(
                visible, masked,
                "hidden physical data changed logical execution"
            );
            let native = visible.0.evaluator.work_units;
            assert!(native > 0);
            for hidden in [false, true] {
                assert!(
                    hidden_attempt(
                        &commit, &txn, &query, &issuer, hidden, returned, native, 1_000_000
                    )
                    .await
                    .is_some()
                );
                assert!(
                    hidden_attempt(
                        &commit,
                        &txn,
                        &query,
                        &issuer,
                        hidden,
                        returned,
                        native - 1,
                        1_000_000
                    )
                    .await
                    .is_none()
                );
            }
            // Find the real signed-work boundary rather than prescribing an
            // implementation-specific number. All failures also prove rollback.
            let (mut low, mut high) = (0, 1_000_000);
            while low < high {
                let middle = low + (high - low) / 2;
                if hidden_attempt(
                    &commit, &txn, &query, &issuer, false, returned, 1_000_000, middle,
                )
                .await
                .is_some()
                {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            assert!(low > 0);
            for hidden in [false, true] {
                assert!(
                    hidden_attempt(
                        &commit, &txn, &query, &issuer, hidden, returned, 1_000_000, low
                    )
                    .await
                    .is_some()
                );
                assert!(
                    hidden_attempt(
                        &commit,
                        &txn,
                        &query,
                        &issuer,
                        hidden,
                        returned,
                        1_000_000,
                        low - 1
                    )
                    .await
                    .is_none()
                );
            }
        }
    });
}

#[test]
fn merge_tail_limits_keep_effects_and_late_failures_restore_the_creation_prefix() {
    lab(0xc4a1_3103, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for rows in [0, 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, rows), NOW)
                .unwrap();
            let program = chain(&format!(
                "UNWIND [1,2] AS x CREATE (a:Visible {{p:x}}), (b:Visible {{p:x+10}}) \
                 MERGE (a)-[e:R {{p:7}}]->(b) RETURN count(e) AS occurrences LIMIT {rows}"
            ));
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
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
                assert_eq!(result.value[0].values(), &[int(2)]);
            }
            assert_eq!(
                (db.vertices().unwrap().len(), db.edges().unwrap().len()),
                (4, 2)
            );
        }
        for (tail, edges) in [
            ("RETURN count(e) AS occurrences", 0),
            ("RETURN 1 / (e.p - 7) AS invalid LIMIT 0", 100),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let rows = u64::from(!tail.ends_with("LIMIT 0"));
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, rows), NOW)
                .unwrap();
            let program = chain(&format!(
                "CREATE (a:Visible {{p:1}}), (b:Visible {{p:2}}) \
                 MERGE (a)-[e:R {{p:7}}]->(b) {tail}"
            ));
            let mut bounded = policy(rows);
            bounded.max_edges = edges;
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
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn explicit_hidden_bindings_and_write_only_merge_refuse_the_entire_chain() {
    lab(0xc4a1_3104, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for (tail, rights, expected) in [
            (
                "MERGE (a)-[e:R]->(b) RETURN e.secret",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[e:R]->(b) RETURN a.secret",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[:R {secret:7}]->(b)",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[e:R]->(b) CREATE (c:Visible {p:e.secret})",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "MERGE (a)-[:R]->(b)",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let token = issuer.issue_at(&grant(rights, 10), NOW).unwrap();
            let program = chain(&format!("CREATE (a:Visible), (b:Visible) {tail}"));
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
            assert_eq!(authorization(&error), Some(expected), "{tail}: {error:?}");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn native_frozen_vertex_merge_profiles_remain_refused_before_any_effect() {
    lab(0xc4a1_3105, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        // This is the native held-creation profile. Supporting relationship
        // steps must not silently opt vertex MERGEs into immediate staging.
        let program = chain("MERGE (a:Visible {p:1}) MERGE (b:Visible {p:2}) MERGE (a)-[:R]->(b)");
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
        let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(&error);
        let mut unsupported = false;
        while let Some(error) = cause {
            unsupported |= matches!(
                error.downcast_ref::<WriteTxnError>(),
                Some(WriteTxnError::AuthorizedChainStepUnsupported { .. })
            );
            cause = error.source();
        }
        assert!(unsupported, "{error:?}");
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
