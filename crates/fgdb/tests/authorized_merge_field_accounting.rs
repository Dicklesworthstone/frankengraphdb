//! Exercise the public atomic program entrypoint, not only a point-read model.
//! Hidden property keys must not change signed/native work refusal thresholds
//! for expressions over admitted fields. Every refusal must discard all steps.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeUpsertPolicy, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramPolicy, PreparedGraphEdgeUpsertQueryText,
    PreparedGraphVertexUpsertText, PreparedGraphWriteProgram,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Restriction, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x57; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn program() -> PreparedGraphWriteProgram {
    PreparedGraphWriteProgram::prepare(
        [
            "MERGE (a:Visible {p:7}) ON MATCH SET a.q = a.p + 1 SET a.q = a.q + 1",
            "MERGE (a:Visible {p:7}) ON MATCH SET a.q = a.q + 1",
        ]
        .into_iter()
        .map(|text| {
            PreparedGraphVertexUpsertText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .into()
        })
        .collect(),
    )
    .unwrap()
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, 100, 1_000_000, 100_000),
        100,
        100,
        100,
    )
}

fn authority() -> (Authority, fgdb_warden::CapabilityToken) {
    let authority =
        Authority::new(AuthKey::from_seed(8157), NS, "graph", SchemaEpoch(1), 1).unwrap();
    let grant = Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 10_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    };
    let token = authority.issue_at(&grant, 100).unwrap();
    (authority, token)
}

async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x56; 32], NS, [0x58; 32]))
        .await
        .unwrap();
    let mut properties = vec![(P, CanonicalScalar::Int(7))];
    if hidden {
        for key in 10..74 {
            properties.push((
                PropertyKeyId(key),
                CanonicalScalar::bytes(vec![0x39; 65]).unwrap(),
            ));
        }
    }
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], properties);
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn authorized_program_expressions_have_identical_work_floors_and_preserve_private_fields() {
    let (_, report) = run_async_under_lab(0xb174, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let (authority, token) = authority();
        let program = program();
        // Separate signed-work and native-evaluator ceilings. A source lookup
        // must not leak through either channel; no quota is raised on refusal.
        for signed in [true, false] {
            let mut floors = Vec::new();
            let mut successful_stats = None;
            let mut boundary_errors = Vec::new();
            for hidden in [false, true] {
                let (mut low, mut high) = (0_u64, 8192_u64);
                while low < high {
                    let allowance = low + (high - low) / 2;
                    let limited = if signed {
                        token.attenuate(Restriction::MaxWork(allowance)).unwrap()
                    } else {
                        token.clone()
                    };
                    let mut limits = policy();
                    if !signed {
                        limits.mutations.query.evaluator.max_work_units = allowance;
                    }
                    let mut db = database(&commit, hidden).await;
                    let before = (db.frontier().unwrap(), db.vertices().unwrap());
                    match db
                        .execute_graph_write_program_returning_authorized(
                            &txn,
                            &query,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            &program,
                            limits,
                            || 100,
                        )
                        .await
                    {
                        Ok((receipt, _)) => {
                            high = allowance;
                            let stats = receipt.stats();
                            assert_eq!(stats.completed_statements, 2);
                            assert_eq!(stats.created_vertices, 0);
                            assert_eq!(stats.mutation_effects, 3);
                            if let Some(expected) = &successful_stats {
                                assert_eq!(&stats, expected, "hidden data changed native receipts");
                            } else {
                                successful_stats = Some(stats);
                            }
                            let row = db.vertex(VId(1)).unwrap().unwrap();
                            assert_eq!(row.props[0], (P, CanonicalScalar::Int(7).into()));
                            assert_eq!(row.props[1], (Q, CanonicalScalar::Int(10).into()));
                            assert_eq!(&row.props[2..], &before.1[0].props[1..]);
                            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                        }
                        Err(_) => {
                            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                            low = allowance + 1;
                        }
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
                assert!(
                    low > 0 && low < 8192,
                    "a real admission boundary must exist"
                );
                floors.push(low);
                // The immediately insufficient budget must refuse identically,
                // not merely fail at two coincidentally equal thresholds.
                let limited = if signed {
                    token.attenuate(Restriction::MaxWork(low - 1)).unwrap()
                } else {
                    token.clone()
                };
                let mut limits = policy();
                if !signed {
                    limits.mutations.query.evaluator.max_work_units = low - 1;
                }
                let mut db = database(&commit, hidden).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap());
                let error = db
                    .execute_graph_write_program_returning_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &limited,
                        "main",
                        &program,
                        limits,
                        || 100,
                    )
                    .await
                    .unwrap_err();
                boundary_errors.push(format!("{error:?}"));
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(successful_stats.is_some());
            assert_eq!(floors[0], floors[1], "signed={signed}");
            assert_eq!(boundary_errors[0], boundary_errors[1], "signed={signed}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn edge_database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x56; 32], NS, [0x58; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
    batch.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(2))]);
    // A single selected pair has TWO existing matches. Both actions and
    // RETURN must retain them; neither admission nor staging is per-commit.
    for (id, value) in [(1, 7), (2, 17)] {
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        if hidden {
            for key in 10..74 {
                properties.push((
                    PropertyKeyId(key),
                    CanonicalScalar::bytes(vec![0x39; 65]).unwrap(),
                ));
            }
        }
        batch.add_edge(EId(id), VId(1), VId(2), properties);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn parallel_edge_actions_and_final_return_share_scope_stable_field_accounting() {
    let (_, report) = run_async_under_lab(0xb175, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let (authority, token) = authority();
        let token = token.attenuate(Restriction::MaxRows(2)).unwrap();
        let query = PreparedGraphEdgeUpsertQueryText::prepare(
            "MATCH (a:Visible),(b:Visible) WHERE a.p=1 AND b.p=2 \
             MERGE (a)-[r:R]->(b) ON MATCH SET r.q=r.p+1 SET r.q=r.q+1 \
             RETURN r AS edge,r.p AS p,r.q AS q ORDER BY edge",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let default_policy = GraphEdgeUpsertPolicy::new(
            GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, 100, 1_000_000, 100_000)),
            100,
        );
        for signed in [true, false] {
            let mut floors = Vec::new();
            let mut expected_stats = None;
            for hidden in [false, true] {
                let (mut low, mut high) = (0_u64, 8192_u64);
                while low < high {
                    let allowance = low + (high - low) / 2;
                    let limited = if signed {
                        token.attenuate(Restriction::MaxWork(allowance)).unwrap()
                    } else {
                        token.clone()
                    };
                    let mut limits = default_policy;
                    if !signed {
                        limits.merge.query.evaluator.max_work_units = allowance;
                    }
                    let mut db = edge_database(&commit, hidden).await;
                    let before = (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap(),
                    );
                    match db
                        .execute_graph_edge_upserts_query_authorized(
                            &txn,
                            &query_cx,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            &query,
                            limits,
                            || 100,
                        )
                        .await
                    {
                        Ok((stats, rows, _)) => {
                            high = allowance;
                            assert_eq!(stats.created_edges, 0);
                            assert_eq!(stats.matched_occurrences, 2);
                            assert_eq!(stats.action_effects, 4);
                            let trace = (stats, rows.rows, rows.evaluator);
                            if let Some(expected) = &expected_stats {
                                assert_eq!(&trace, expected);
                            } else {
                                expected_stats = Some(trace);
                            }
                            assert_eq!(rows.value.len(), 2);
                            for (row, (id, p)) in rows.value.iter().zip([(1, 7), (2, 17)]) {
                                assert_eq!(
                                    row.values(),
                                    &[
                                        GraphValue::Edge(EId(id)),
                                        GraphValue::Scalar(CanonicalScalar::Int(p)),
                                        GraphValue::Scalar(CanonicalScalar::Int(p + 2)),
                                    ]
                                );
                                let after = db.edge(EId(id)).unwrap().unwrap();
                                assert_eq!(after.props[0], (P, CanonicalScalar::Int(p).into()));
                                assert_eq!(after.props[1], (Q, CanonicalScalar::Int(p + 2).into()));
                                assert_eq!(after.props.len(), if hidden { 66 } else { 2 });
                                for (key, value) in &after.props[2..] {
                                    assert!((10..74).contains(&key.0));
                                    assert_eq!(
                                        *value,
                                        CanonicalScalar::bytes(vec![0x39; 65]).unwrap().into()
                                    );
                                }
                            }
                            assert_eq!(db.vertices().unwrap(), before.1);
                            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                        }
                        Err(_) => {
                            assert_eq!(
                                (
                                    db.frontier().unwrap(),
                                    db.vertices().unwrap(),
                                    db.edges().unwrap(),
                                ),
                                before
                            );
                            low = allowance + 1;
                        }
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
                assert!(low > 0 && low < 8192);
                floors.push(low);
            }
            assert!(expected_stats.is_some());
            assert_eq!(floors[0], floors[1], "signed={signed}");
        }
        // A real final-row refusal still rolls back BOTH parallel updates.
        let limited = token.attenuate(Restriction::MaxRows(1)).unwrap();
        let mut db = edge_database(&commit, true).await;
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        assert!(
            db.execute_graph_edge_upserts_query_authorized(
                &txn,
                &query_cx,
                &commit,
                &authority,
                &limited,
                "main",
                &query,
                default_policy,
                || 100,
            )
            .await
            .is_err()
        );
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
