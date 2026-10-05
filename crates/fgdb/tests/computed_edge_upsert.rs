//! Computed relationship actions use the real native transaction and Chronicle path.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphEdgeMergePolicy,
    GraphEdgeUpsertAction, GraphEdgeUpsertBranch, GraphEdgeUpsertBuildError, GraphEdgeUpsertError,
    GraphEdgeUpsertPolicy, GraphEdgeUpsertValue, GraphIntegerBinary, GraphIntegerExpression,
    GraphIntegerOp, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeText, PreparedGraphEdgeUpsert,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Action = GraphEdgeUpsertAction<GraphEdgeUpsertValue>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn merge() -> PreparedGraphEdgeMerge {
    PreparedGraphEdgeMergeText::prepare(
        "MATCH (a:Node {p:1}),(b:Node {p:2}) MERGE (a)-[:R]->(b)",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn literal(key: PropertyKeyId, value: i64) -> Action {
    GraphEdgeUpsertAction {
        key,
        value: GraphEdgeUpsertValue::Literal(
            GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap(),
        ),
    }
}
fn expression(key: PropertyKeyId, properties: &[PropertyKeyId], ops: &[GraphIntegerOp]) -> Action {
    GraphEdgeUpsertAction {
        key,
        value: GraphEdgeUpsertValue::Expression {
            properties: properties.to_vec(),
            value: GraphIntegerExpression::prepare_scalar(ops).unwrap(),
        },
    }
}
fn copy(key: PropertyKeyId, from: PropertyKeyId) -> Action {
    expression(key, &[from], &[GraphIntegerOp::ScalarColumn(0)])
}
fn increment() -> Action {
    expression(
        P,
        &[P],
        &[
            GraphIntegerOp::ScalarColumn(0),
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Add),
        ],
    )
}
fn invalid_value() -> Action {
    expression(
        P,
        &[],
        &[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ],
    )
}
fn policy(actions: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000)),
        actions,
    )
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
    batch.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(2))]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn create_match_simultaneous_assignments_and_trailing_set_survive_reopen() {
    let ((), report) = run_async_under_lab(0xed6e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let before = db.frontier().unwrap();
        let definition = PreparedGraphEdgeUpsert::prepare_with_clauses(
            merge(),
            vec![copy(P, Q), copy(Q, P)],
            vec![literal(P, 10), literal(Q, 20)],
            vec![increment()],
        )
        .unwrap();
        let mut allocations = 0;
        for expected in [[11, 20], [21, 11]] {
            let (stats, outcome, completion) = db
                .execute_graph_edge_upsert_autocommit_governed(
                    &txcx,
                    &query,
                    &commit,
                    &definition,
                    policy(3),
                    |_| {
                        allocations += 1;
                        assert_eq!(allocations, 1, "the matched arm cannot allocate");
                        Ok::<_, ()>(ElementId::Edge(EId(10)))
                    },
                )
                .await
                .unwrap();
            assert_eq!(outcome.edge(), Some(EId(10)));
            assert_eq!(stats.action_effects, 3);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            let edge = db.edge(EId(10)).unwrap().unwrap();
            assert_eq!(
                edge.props,
                vec![
                    (P, CanonicalScalar::Int(expected[0])),
                    (Q, CanonicalScalar::Int(expected[1]))
                ]
            );
        }
        assert_eq!(allocations, 1);
        assert_eq!(db.frontier().unwrap().0, before.0 + 2);
        assert!(pinned.edge(EId(10)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(21)), (Q, CanonicalScalar::Int(11))]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_expression_failure_restores_the_outer_workspace_prefix() {
    let ((), report) = run_async_under_lab(0xed6e_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(77)));
        txn.write(&mut db, prefix).unwrap();
        let definition = PreparedGraphEdgeUpsert::prepare_with_clauses(
            merge(),
            vec![],
            vec![literal(P, 10)],
            vec![invalid_value()],
        )
        .unwrap();
        let error = txn
            .execute_graph_edge_upsert_governed(&mut db, &query, &definition, policy(2), |_| {
                Ok::<_, ()>(ElementId::Edge(EId(10)))
            })
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                clause: 1,
                action: 0,
                ..
            })
        ));
        assert!(txn.edge(&db, EId(10)).unwrap().is_none());
        assert_eq!(
            txn.vertex_property(&db, VId(1), Q).unwrap(),
            Some(CanonicalScalar::Int(77))
        );
        assert_eq!(db.frontier().unwrap(), before);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_input_skips_all_clauses_and_unselected_match_failures_stay_lazy() {
    let ((), report) = run_async_under_lab(0xed6e_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let definition = PreparedGraphEdgeUpsert::prepare_with_clauses(
            merge(),
            vec![invalid_value()],
            vec![invalid_value()],
            vec![invalid_value()],
        )
        .unwrap();
        let before = db.frontier().unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_edge_upsert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                policy(0),
                |_| -> Result<ElementId, ()> { panic!("NoInput cannot allocate") },
            )
            .await
            .unwrap();
        assert_eq!(stats.branch, GraphEdgeUpsertBranch::NoInput);
        assert_eq!(stats.action_effects, 0);
        assert!(outcome.edge().is_none());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        seed(&mut db, &commit).await;
        let definition = PreparedGraphEdgeUpsert::prepare_with_clauses(
            merge(),
            vec![invalid_value()],
            vec![literal(P, 4)],
            vec![increment()],
        )
        .unwrap();
        db.execute_graph_edge_upsert_autocommit_governed(
            &txcx,
            &query,
            &commit,
            &definition,
            policy(2),
            |_| Ok::<_, ()>(ElementId::Edge(EId(10))),
        )
        .await
        .unwrap();
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(5))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_clauses_share_action_and_evaluator_allowances() {
    let ((), report) = run_async_under_lab(0xed6e_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let definition = PreparedGraphEdgeUpsert::prepare_with_clauses(
            merge(),
            vec![],
            vec![literal(P, 10)],
            vec![increment()],
        )
        .unwrap();
        let mut measured = None;
        for run in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let before = db.frontier().unwrap();
            let mut allowance = policy(if run == 1 { 1 } else { 2 });
            if let Some(usage) = measured {
                let usage: fgdb_gql::GlaExecutionStats = usage;
                if run >= 2 {
                    allowance.merge.query.evaluator.max_work_units =
                        usage.work_units - u64::from(run == 3);
                    allowance.merge.query.evaluator.max_scratch_entries =
                        usage.scratch_entries - u64::from(run == 4);
                }
            }
            let result = db
                .execute_graph_edge_upsert_autocommit_governed(
                    &txcx,
                    &query,
                    &commit,
                    &definition,
                    allowance,
                    |_| Ok::<_, ()>(ElementId::Edge(EId(10))),
                )
                .await;
            if run == 0 || run == 2 {
                let (stats, _, _) = result.unwrap();
                measured = Some(stats.evaluator);
            } else {
                let error = result.unwrap_err();
                if run == 1 {
                    assert!(matches!(
                        error,
                        GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                            limit: 1,
                            observed: 2,
                        })
                    ));
                } else {
                    assert!(matches!(error, GqlQueryError::Evaluator(_)));
                }
                assert!(db.edge(EId(10)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap(), before);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expression_inputs_and_clause_transcripts_are_admitted_without_execution() {
    let invalid = expression(P, &[P], &[GraphIntegerOp::ScalarColumn(1)]);
    assert!(matches!(
        PreparedGraphEdgeUpsert::prepare_with_clauses(merge(), vec![invalid], vec![], vec![],),
        Err(GraphEdgeUpsertBuildError::InvalidExpressionInput { .. })
    ));
    let first = PreparedGraphEdgeUpsert::prepare_with_clauses(
        merge(),
        vec![literal(P, 1)],
        vec![],
        vec![literal(P, 2)],
    )
    .unwrap();
    let second =
        PreparedGraphEdgeUpsert::prepare_with_clauses(merge(), vec![literal(P, 2)], vec![], vec![])
            .unwrap();
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(first.action_count(GraphEdgeUpsertBranch::Match), 2);
    assert_eq!(first.action_count(GraphEdgeUpsertBranch::NoInput), 0);
    let read_p =
        PreparedGraphEdgeUpsert::prepare_with_clauses(merge(), vec![copy(Q, P)], vec![], vec![])
            .unwrap();
    let read_q =
        PreparedGraphEdgeUpsert::prepare_with_clauses(merge(), vec![copy(Q, Q)], vec![], vec![])
            .unwrap();
    assert_ne!(read_p.canonical_bytes(), read_q.canonical_bytes());
}
