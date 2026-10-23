//! Native vertex and relationship MERGE chains share scope, work and one completion.
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
    CanonicalF64, CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId,
    EId, EmbeddedTxnCompletion, PurposeContexts, QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Restriction, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
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
        properties: Scope::only([P, Q]),
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
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn chain(text: &str) -> PreparedGraphWriteChain {
    chain_with(text, &GqlParameters::new())
}

fn chain_with(text: &str, parameters: &GqlParameters) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    let template = PreparedGraphWriteChainText::prepare(text, R, symbols).expect(text);
    let result = template.bind_parameters(parameters).expect(text);
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
fn frozen_parameter_merges_preserve_exact_keys_and_reopen_one_idempotent_import() {
    lab(0xc4a1_3105, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let large = 1_i64 << 53;
        let float = |value| GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)));
        let pairs = vec![
            (int(1), int(2)),
            (int(2), int(3)),
            (int(1), int(2)),
            (int(3), int(3)),
            (float(1.0), int(2)),
            (int(large + 1), int(0)),
            (float(large as f64), int(0)),
        ];
        let parameters = GqlParameters::new()
            .with_list(
                "rows",
                pairs
                    .into_iter()
                    .map(|(source, target)| {
                        GraphValue::map(vec![("s".into(), source), ("d".into(), target)]).unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        let original = parameters.canonical_bytes();
        let statement = "UNWIND $rows AS row MERGE (a:Visible {p:row.s}) \
             MERGE (b:Visible {p:row.d}) MERGE (a)-[:R]->(b)";
        let program = chain_with(statement, &parameters);
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
        assert_eq!((stats.created_vertices, stats.created_edges), (6, 5));
        assert!(result.value.is_empty());
        assert_eq!(result.rows.result_rows, 0);
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        let property = |vertex| {
            vertices.iter().find(|row| row.vid == vertex).unwrap().props[0]
                .1
                .clone()
        };
        let integer = |value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value));
        let mut actual: Vec<_> = edges
            .iter()
            .map(|edge| (property(edge.entry.src), property(edge.entry.dst)))
            .collect();
        let mut expected = vec![
            (integer(1), integer(2)),
            (integer(2), integer(3)),
            (integer(3), integer(3)),
            (integer(large + 1), integer(0)),
            (
                CanonicalScalar::Float(CanonicalF64::new(large as f64)).into(),
                integer(0),
            ),
        ];
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "rounded numeric buckets must not become equality"
        );
        let mut no_creation = policy(0);
        no_creation.max_vertices = 0;
        no_creation.max_edges = 0;
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                no_creation,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(result.value.is_empty());
        let empty = chain_with(
            statement,
            &GqlParameters::new().with_list("rows", Vec::new()).unwrap(),
        );
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &empty,
                no_creation,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(result.value.is_empty());
        assert_eq!(parameters.canonical_bytes(), original);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
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
fn vertex_merge_bags_keep_all_visible_matches_before_final_paging() {
    lab(0xc4a1_3106, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for (tail, public_rows) in [
            ("RETURN a ORDER BY a", 4),
            ("RETURN count(*) AS copies", 1),
            ("RETURN count(*) AS copies LIMIT 0", 0),
        ] {
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, public_rows), NOW)
                .unwrap();
            let program = chain(&format!(
                "UNWIND [7,7] AS x MERGE (a:Visible {{p:x}}) \
                 MERGE (b:Visible {{p:9}}) {tail}"
            ));
            let mut visible = None;
            for hidden in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                for id in [1, 3] {
                    let mut properties = vec![(P, CanonicalScalar::Int(7))];
                    if hidden {
                        properties.push((SECRET, CanonicalScalar::Int(99)));
                    }
                    seed.create_vertex(VId(id), vec![L], properties);
                }
                if hidden {
                    seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(7))]);
                }
                db.write(&commit, seed).await.unwrap();
                let before = db.frontier().unwrap();
                let (stats, result, _) = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(public_rows),
                        || NOW,
                    )
                    .await
                    .unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
                assert_eq!(result.rows.result_rows, public_rows);
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                if public_rows == 4 {
                    assert_eq!(
                        result
                            .value
                            .iter()
                            .map(GraphValueRow::values)
                            .collect::<Vec<_>>(),
                        vec![
                            &[GraphValue::Vertex(VId(1))][..],
                            &[GraphValue::Vertex(VId(1))][..],
                            &[GraphValue::Vertex(VId(3))][..],
                            &[GraphValue::Vertex(VId(3))][..],
                        ],
                    );
                } else if public_rows == 1 {
                    assert_eq!(result.value[0].values(), &[int(4)]);
                } else {
                    assert!(result.value.is_empty());
                }
                let observed = (stats, result.value);
                if let Some(expected) = &visible {
                    assert_eq!(
                        &observed, expected,
                        "hidden matches and fields cannot affect work"
                    );
                } else {
                    visible = Some(observed);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn frozen_import_work_does_not_rescan_the_growing_creation_prefix() {
    lab(0xc4a1_3107, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        for count in [64_i64, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let input = GqlParameters::new()
                .with_list(
                    "rows",
                    (0..count * 2)
                        .map(|at| {
                            GraphValue::map(vec![
                                ("s".into(), int(at % count)),
                                ("d".into(), int((at + 1) % count)),
                            ])
                            .unwrap()
                        })
                        .collect(),
                )
                .unwrap();
            let program = chain_with(
                "UNWIND $rows AS row MERGE (a:Visible {p:row.s}) \
                 MERGE (b:Visible {p:row.d}) MERGE (a)-[:R]->(b)",
                &input,
            );
            // A sequential growing-overlay scan exceeds this linear record
            // allowance. The entry snapshot plus held creations stays within it.
            let bounded = GraphInsertPolicy::new(
                GqlQueryPolicy::new(16 * count as u64, 0, 1_000_000, 100_000),
                count as u64,
                count as u64,
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
                    bounded,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                (count as u64, count as u64)
            );
            assert!(stats.selection.snapshot_records <= 16 * count as u64);
            assert!(result.value.is_empty());
            assert_eq!(db.vertices().unwrap().len(), count as usize);
            assert_eq!(db.edges().unwrap().len(), count as usize);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn frozen_on_create_changes_are_seen_by_later_merges_and_final_creation() {
    lab(0xc4a1_3108, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let program = chain(
            "UNWIND [1,2] AS x MERGE (a:Visible {p:x}) ON CREATE SET a.q=7,a.p=x+1 \
             MERGE (b:Visible {p:x}) CREATE (a)-[:R]->(b)",
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
                policy(0).with_mutation_limit(2),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        assert!(result.value.is_empty());
        let vertices = db.vertices().unwrap();
        let first = vertices
            .iter()
            .find(|vertex| {
                vertex.props
                    == vec![
                        (P, CanonicalScalar::Int(2).into()),
                        (Q, CanonicalScalar::Int(7).into()),
                    ]
            })
            .unwrap();
        let second = vertices
            .iter()
            .find(|vertex| vertex.props == vec![(P, CanonicalScalar::Int(1).into())])
            .unwrap();
        let mut pairs: Vec<_> = db
            .edges()
            .unwrap()
            .iter()
            .map(|edge| (edge.entry.src, edge.entry.dst))
            .collect();
        pairs.sort();
        let mut expected = vec![(first.vid, first.vid), (first.vid, second.vid)];
        expected.sort();
        assert_eq!(pairs, expected);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn duplicate_merge_actions_run_per_occurrence_with_clause_entry_rhs_values() {
    lab(0xc4a1_3109, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain(
            "UNWIND [1,1,1] AS x MERGE (a:Visible {p:x}) \
             ON CREATE SET a.q=0 ON MATCH SET a.q=a.q+1 SET a.q=a.q+10 \
             MERGE (b:Visible {p:2}) RETURN a.q AS q",
        );
        for limit in [6, 5] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(3).with_mutation_limit(limit),
                    || NOW,
                )
                .await;
            if limit == 6 {
                let (stats, result, _) = result.unwrap();
                assert_eq!(stats.created_vertices, 2);
                assert_eq!(result.value.len(), 3);
                for row in result.value {
                    assert_eq!(row.values(), &[int(32)]);
                }
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            } else {
                assert!(
                    result.is_err(),
                    "all six original actions share the statement cap"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(7))],
        );
        db.write(&commit, seed).await.unwrap();
        let program = chain(
            "CREATE (marker:Visible {p:99}) MERGE (a:Visible {p:1}) \
             ON MATCH SET a.p=a.q,a.q=a.p RETURN a.p AS p,a.q AS q",
        );
        let (_, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1).with_mutation_limit(2),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value[0].values(),
            &[int(7), int(1)],
            "SET RHS reads precede the clause's writes"
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Int(1).into())
            ]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn vertex_merge_scope_checks_cover_noop_actions_keys_and_carried_fields() {
    lab(0xc4a1_3110, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(1)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for text in [
            "CREATE (marker:Visible) MERGE (a:Visible {p:1}) ON MATCH SET a.secret=99",
            "CREATE (marker:Visible) MERGE (a:Visible {p:1}) ON MATCH SET a:Hidden",
            "CREATE (marker:Visible) MERGE (a:Visible {secret:99})",
            "CREATE (marker:Visible) MERGE (a:Hidden {p:1})",
            "CREATE (marker:Visible) MERGE (a:Visible {p:1}) RETURN a.secret",
            "MERGE (a:Visible {p:4}) ON CREATE SET a.secret=99 MERGE (b:Visible {p:5})",
            "MERGE (a:Visible {p:4}) ON CREATE SET a:Hidden MERGE (b:Visible {p:5})",
        ] {
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(10),
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
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        // An unselected ON branch is not an intent. The same hidden field
        // that refuses above must not deny a branch that never executes.
        for (text, created) in [
            (
                "MERGE (a:Visible {p:1}) ON CREATE SET a.secret=0 \
                 MERGE (b:Visible {p:1}) RETURN count(*) AS copies",
                0,
            ),
            (
                "MERGE (a:Visible {p:4}) ON MATCH SET a.secret=0 \
                 MERGE (b:Visible {p:4}) RETURN count(*) AS copies",
                1,
            ),
        ] {
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(1).with_mutation_limit(0),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.created_vertices, created);
            assert_eq!(result.value[0].values(), &[int(1)]);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap(), before.1[0]);
    });
}

#[test]
fn vertex_merge_native_limits_and_late_expiry_or_output_refusal_abort_every_step() {
    lab(0xc4a1_3111, |contexts| async move {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "UNWIND [1,2] AS x MERGE (a:Visible {p:x}) \
             MERGE (b:Visible {p:x+10}) MERGE (a)-[e:R]->(b) RETURN count(e) AS copies",
        );
        let mut measured = None;
        for attempt in 0..6 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut bounded = policy(1);
            if let Some((work, scratch)) = measured {
                bounded.query = GqlQueryPolicy::new(
                    10_000,
                    1,
                    work - u64::from(attempt == 2),
                    scratch - u64::from(attempt == 3),
                );
            }
            if attempt == 4 {
                bounded.max_vertices = 3;
            } else if attempt == 5 {
                bounded.max_edges = 1;
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
                let (stats, result, _) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
                assert_eq!(result.value[0].values(), &[int(2)]);
                let observed = (
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                );
                assert!(observed.0 > 0 && observed.1 > 0);
                if let Some(expected) = measured {
                    assert_eq!(observed, expected, "exact cumulative limits still succeed");
                }
                measured = Some(observed);
            } else {
                assert!(result.is_err(), "attempt {attempt} must refuse");
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        for expire in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let now = Arc::new(AtomicU64::new(NOW));
            let admission_clock = Arc::clone(&now);
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
                    || now.load(Ordering::Relaxed),
                    |rows, meter| {
                        called += 1;
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].values(), &[int(2)]);
                        meter(1)?;
                        if expire {
                            admission_clock.store(10_000, Ordering::Relaxed);
                            Ok(())
                        } else {
                            Err(WriteTxnError::Authorization(Error::ScopeDenied))
                        }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(called, 1, "the complete tentative result was reached");
            assert_eq!(
                authorization(&error),
                Some(if expire {
                    Error::Expired
                } else {
                    Error::ScopeDenied
                })
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn hidden_merge_action_inputs_are_null_before_reading_and_do_not_change_work() {
    lab(0xc4a1_3112, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "CREATE (marker:Visible {p:2}) MERGE (a:Visible {p:1}) \
             ON MATCH SET a.q=a.secret MERGE (b:Visible {p:2}) RETURN a.q AS q",
        );
        let mut absent = None;
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            let mut properties = vec![(P, CanonicalScalar::Int(1))];
            if hidden {
                properties.push((SECRET, CanonicalScalar::Int(99)));
            }
            properties.push((Q, CanonicalScalar::Int(7)));
            seed.create_vertex(VId(1), vec![L], properties);
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let (stats, result, _) = db
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
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Null)]
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let stored = db.vertex(VId(1)).unwrap().unwrap();
            assert!(
                !stored.props.iter().any(|(key, _)| *key == Q),
                "masked NULL removes the old allowed value"
            );
            assert_eq!(
                stored
                    .props
                    .iter()
                    .find(|(key, _)| *key == SECRET)
                    .map(|(_, value)| value),
                hidden.then_some(&CanonicalPropertyValue::Scalar(CanonicalScalar::Int(99))),
                "the hidden source field must remain unchanged",
            );
            let mut visible_graph: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .map(|vertex| {
                    let properties: Vec<_> = vertex
                        .props
                        .into_iter()
                        .filter(|(key, _)| *key != SECRET)
                        .collect();
                    (vertex.labels, properties)
                })
                .collect();
            visible_graph.sort();
            let observed = (stats, result.value, visible_graph);
            if let Some(expected) = &absent {
                assert_eq!(
                    &observed, expected,
                    "hidden and absent inputs have identical observable work and effects"
                );
            } else {
                absent = Some(observed);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn terminal_vertex_merge_rows_stay_private_with_and_without_frozen_execution() {
    lab(0xc4a1_3113, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        for actions in ["", " ON MATCH SET a.secret=99"] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let program = chain(&format!(
                "UNWIND [1,2] AS x MERGE (a:Visible {{p:x}}){actions} \
                 MERGE (b:Visible {{p:x+10}})"
            ));
            assert!(!program.returns_rows());
            let mut admitted = 0;
            let (stats, result, completion) = db
                .execute_graph_write_chain_authorized_checked(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0).with_mutation_limit(0),
                    || NOW,
                    |rows, meter| {
                        admitted += 1;
                        assert!(
                            rows.is_empty(),
                            "private continuation identities must not reach the host"
                        );
                        meter(1)
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                admitted, 1,
                "no-output statements still admit their empty representation"
            );
            assert_eq!((stats.created_vertices, stats.created_edges), (4, 0));
            assert_eq!(stats.selection.result_rows, 0);
            assert_eq!(result.rows.result_rows, 0);
            assert!(result.value.is_empty());
            assert_eq!(db.vertices().unwrap().len(), 4);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: db.frontier().unwrap()
                }
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
