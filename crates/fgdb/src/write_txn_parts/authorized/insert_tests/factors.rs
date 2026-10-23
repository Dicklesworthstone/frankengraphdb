//! Authorized bulk endpoints: one visible source, native staged effects and
//! unchanged publication boundaries. No independently granted factor permits.

use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlBudgetDimension, GraphWriteProgramError, GraphWriteProgramPolicy,
    PreparedGraphInsertQueryText, PreparedGraphMutationText, PreparedGraphWriteProgram,
};

const BULK: &str = "UNWIND $rows AS row \
    MATCH (a:Visible {p:row.src}), (b:Visible {p:row.dst}) \
    CREATE (a)-[:R {p:row.weight}]->(b)";

fn parameters(rows: &[(i64, i64, i64)]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            rows.iter()
                .map(|&(source, destination, weight)| {
                    GraphValue::map(vec![
                        (
                            "src".into(),
                            GraphValue::Scalar(CanonicalScalar::Int(source)),
                        ),
                        (
                            "dst".into(),
                            GraphValue::Scalar(CanonicalScalar::Int(destination)),
                        ),
                        (
                            "weight".into(),
                            GraphValue::Scalar(CanonicalScalar::Int(weight)),
                        ),
                    ])
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
}

fn bound(rows: &[(i64, i64, i64)]) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(BULK, R, symbols)
        .unwrap()
        .bind_parameters(&parameters(rows))
        .unwrap()
}

async fn fixture(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for (id, key) in [(1, 1), (2, 1), (3, 2)] {
        let mut labels = vec![L];
        let mut properties = vec![(P, CanonicalScalar::Int(key))];
        if hidden {
            labels.push(HIDDEN);
            properties.push((SECRET, CanonicalScalar::Int(777)));
        }
        seed.create_vertex(VId(id), labels, properties);
    }
    if hidden {
        for (id, key) in [(900, 99), (901, 1), (902, 2)] {
            seed.create_vertex(VId(id), vec![HIDDEN], vec![(P, CanonicalScalar::Int(key))]);
        }
    }
    db.write(cx, seed).await.unwrap();
    if hidden {
        let mut relation = WriteBatch::new(H);
        relation.add_edge(
            EId(900),
            VId(900),
            VId(1),
            vec![(SECRET, CanonicalScalar::Int(7))],
        );
        db.write(cx, relation).await.unwrap();
    }
    db
}

#[test]
fn zero_match_bulk_factors_charge_one_visible_node_domain_under_the_product_work_bound() {
    lab(0xa95a, |contexts| async move {
        const N: u64 = 512;
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority
            .issue_at(&read_write_grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxNodes(N))
            .unwrap()
            .attenuate(Restriction::MaxRows(0))
            .unwrap();
        let insertion = bound(&[(1, 9999, 7), (2, 9999, 8)]);
        let bounded = GraphInsertPolicy::new(GqlQueryPolicy::new(N, 0, 200_000, 100_000), 0, 0);
        assert!(bounded.query.evaluator.max_work_units < N * N);
        let mut observations = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for key in 0..N {
                let mut properties = vec![(P, CanonicalScalar::Int(key as i64))];
                if hidden {
                    properties.push((SECRET, CanonicalScalar::Int(key as i64)));
                }
                seed.create_vertex(VId(u128::from(key) + 1), vec![L], properties);
            }
            if hidden {
                for key in 0..17 {
                    // Hidden matches must not turn the empty visible product
                    // into an edge proposal or consume any signed node units.
                    seed.create_vertex(
                        VId(900 + key),
                        vec![HIDDEN],
                        vec![(P, CanonicalScalar::Int(9999))],
                    );
                }
            }
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let fewer_nodes = token.attenuate(Restriction::MaxNodes(N - 1)).unwrap();
            let error = db
                .execute_graph_insert_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &fewer_nodes,
                    "main",
                    &insertion,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(error),
                Error::LimitExceeded(LimitDimension::Nodes)
            );
            let native_limit =
                GraphInsertPolicy::new(GqlQueryPolicy::new(N - 1, 0, 200_000, 100_000), 0, 0);
            assert!(matches!(db.execute_graph_insert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &insertion, native_limit, || NOW,
            ).await, Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords));
            let (stats, completion) = db
                .execute_graph_insert_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &insertion,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.selection.snapshot_records, N);
            assert_eq!(stats.selection.result_rows, 0);
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::ReadClosed {
                    snapshot_seq: before,
                    validated_through: before,
                }
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
            observations.push(stats);
        }
        assert_eq!(
            observations[0], observations[1],
            "hidden fields and matches changed visible usage"
        );
    });
}

#[test]
fn authorized_factor_creation_preserves_occurrences_masked_return_values_and_row_quota_rollback() {
    lab(0xa95b, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let query = PreparedGraphInsertQueryText::prepare(
            &format!("{BULK} RETURN a,b,a.secret AS secret,row.weight AS weight"),
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&parameters(&[
            (1, 2, 11),
            (2, 1, 12),
            (1, 2, 13),
            (99, 1, 14),
        ]))
        .unwrap();
        let bounded = GraphInsertPolicy::new(GqlQueryPolicy::new(3, 6, 200_000, 100_000), 0, 6);
        let pairs = [
            (1, 3, 11),
            (2, 3, 11),
            (3, 1, 12),
            (3, 2, 12),
            (1, 3, 13),
            (2, 3, 13),
        ];
        let mut observations = Vec::new();
        for hidden in [false, true] {
            let mut db = fixture(&commit, hidden).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let limited = token.attenuate(Restriction::MaxRows(5)).unwrap();
            let error = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &limited,
                    "main",
                    &query,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                query_authorization(&error),
                Some(Error::LimitExceeded(LimitDimension::Rows))
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

            // A fresh handle/fixture also makes the occurrence-to-identity
            // correspondence explicit; refused reservations are not reclaimed.
            let mut db = fixture(&commit, hidden).await;
            let before = db.frontier().unwrap();
            let (stats, result, completion) = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query_cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    bounded,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.selection.snapshot_records, 3);
            assert_eq!((stats.created_vertices, stats.created_edges), (0, 6));
            assert_eq!(result.rows.result_rows, 6);
            assert_eq!(result.rows.snapshot_records, 3);
            for (at, (source, destination, weight)) in pairs.into_iter().enumerate() {
                assert_eq!(
                    result.value[at].values(),
                    &[
                        GraphValue::Vertex(VId(source)),
                        GraphValue::Vertex(VId(destination)),
                        GraphValue::Scalar(CanonicalScalar::Null),
                        GraphValue::Scalar(CanonicalScalar::Int(weight)),
                    ]
                );
                let edge = db
                    .edge(crate::write_txn::engine_edge(&keys(), at as u64 + 1))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    (edge.entry.src, edge.entry.relation, edge.entry.dst),
                    (VId(source), R, VId(destination))
                );
                assert_eq!(
                    edge.props,
                    vec![(
                        P,
                        fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(weight))
                    )]
                );
            }
            let after = db.frontier().unwrap();
            assert_eq!(after.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { commit_seq: after }
            );
            assert_eq!(txn.outstanding_obligations(), 0);
            observations.push((stats, result));
        }
        assert_eq!(
            observations[0], observations[1],
            "hidden data changed rows or governed execution"
        );
    });
}

#[test]
fn authorized_factor_program_reads_rekeyed_and_created_overlay_rows_and_rolls_back_the_prefix() {
    lab(0xa95c, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&read_write_grant(), NOW).unwrap();
        let mutation =
            PreparedGraphMutationText::prepare("MATCH (n:Visible {p:2}) SET n.p = 20", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            mutation.into(),
            insert("CREATE (:Visible {p:9})").into(),
            bound(&[(1, 20, 7), (20, 9, 8), (1, 2, 9)]).into(),
        ])
        .unwrap();
        for (record_limit, edge_limit) in [(4, 2), (4, 3), (5, 2), (5, 3), (7, 2), (7, 3)] {
            let mut db = fixture(&commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let policy = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(record_limit, 100, 1_000_000, 100_000),
                1,
                1,
                edge_limit,
            );
            let result = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    policy,
                    || NOW,
                )
                .await;
            if record_limit == 4 {
                assert!(matches!(
                    result,
                    Err(GraphWriteProgramError::Program(
                        fgdb_gql::GraphMutationProgramError::Budget {
                            statement: 2,
                            dimension: fgdb_gql::GraphMutationProgramDimension::SnapshotRecords,
                            limit: 4,
                            observed: 5,
                        }
                    ))
                ));
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            } else if edge_limit == 2 {
                assert!(matches!(
                    result,
                    Err(GraphWriteProgramError::CreationBudget {
                        statement: 2,
                        dimension: GraphInsertLimitDimension::Edges,
                        limit: 2,
                        observed: 3,
                    })
                ));
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            } else {
                let (receipt, completion) = result.unwrap();
                let stats = receipt.stats();
                assert_eq!(stats.completed_statements, 3);
                assert_eq!(
                    (
                        stats.mutation_effects,
                        stats.created_vertices,
                        stats.created_edges
                    ),
                    (1, 1, 3)
                );
                assert_eq!(
                    stats.selection.snapshot_records, 5,
                    "one equality-selected basis vertex then four overlay vertices, each admitted once"
                );
                let created = receipt.steps()[1].created_vertices().unwrap()[0];
                assert_eq!(
                    db.vertex(VId(3)).unwrap().unwrap().props,
                    vec![
                        (
                            P,
                            fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(20))
                        ),
                        (
                            SECRET,
                            fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(777))
                        )
                    ]
                );
                assert_eq!(
                    db.vertex(created).unwrap().unwrap().props,
                    vec![(
                        P,
                        fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(9))
                    )]
                );
                let expected = [
                    (VId(1), VId(3), 7),
                    (VId(2), VId(3), 7),
                    (VId(3), created, 8),
                ];
                let edges = receipt.steps()[2].created_edges().unwrap();
                assert_eq!(edges.len(), 3);
                for (&id, (source, destination, weight)) in edges.iter().zip(expected) {
                    let edge = db.edge(id).unwrap().unwrap();
                    assert_eq!(
                        (edge.entry.src, edge.entry.relation, edge.entry.dst),
                        (source, R, destination)
                    );
                    assert_eq!(
                        edge.props,
                        vec![(
                            P,
                            fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(weight))
                        )]
                    );
                }
                let after = db.frontier().unwrap();
                assert_eq!(after.0, before.0.0 + 1);
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq: after }
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
