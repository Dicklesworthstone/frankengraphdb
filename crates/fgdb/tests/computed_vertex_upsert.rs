//! Computed MERGE actions use the real native overlay, meter and completion.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerBinary as Binary,
    GraphIntegerExpression, GraphIntegerOp as Op, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertAction as Action, GraphVertexUpsertBuildError,
    GraphVertexUpsertError, GraphVertexUpsertPolicy, PreparedGraphVertexMerge,
    PreparedGraphVertexMergeText, PreparedGraphVertexUpsert,
};
use fgdb_types::{CanonicalScalar as Scalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const COUNT: PropertyKeyId = PropertyKeyId(2);
const A: PropertyKeyId = PropertyKeyId(3);
const B: PropertyKeyId = PropertyKeyId(4);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        _ => None,
    }
}
fn merge() -> PreparedGraphVertexMerge {
    PreparedGraphVertexMergeText::prepare("MERGE (n:Person {id:1})", R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn expression(key: PropertyKeyId, properties: &[PropertyKeyId], ops: &[Op]) -> Action {
    Action::SetExpression {
        key,
        properties: properties.to_vec(),
        value: GraphIntegerExpression::prepare_scalar(ops).unwrap(),
    }
}
fn increment(key: PropertyKeyId) -> Action {
    expression(
        key,
        &[key],
        &[
            Op::ScalarColumn(0),
            Op::Literal(Some(0)),
            Op::Coalesce,
            Op::Literal(Some(1)),
            Op::Binary(Binary::Add),
        ],
    )
}
fn fail(key: PropertyKeyId) -> Action {
    expression(
        key,
        &[],
        &[
            Op::Literal(Some(1)),
            Op::Literal(Some(0)),
            Op::Binary(Binary::Divide),
        ],
    )
}
fn policy(work: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(20_000, 20_000, work, 2_000_000)),
        16,
    )
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(1),
        vec![L],
        vec![
            (ID, Scalar::Int(1)),
            (COUNT, Scalar::Int(0)),
            (A, Scalar::Int(5)),
            (B, Scalar::Int(9)),
        ],
    );
    batch
}
fn scalar(db: &Database<MemVfs>, key: PropertyKeyId) -> Scalar {
    db.vertex(VId(1))
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(actual, _)| *actual == key)
        .unwrap()
        .1
}

#[test]
fn simultaneous_rhs_and_sequential_clauses_survive_reopen() {
    let ((), report) = run_async_under_lab(0x5c73_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let upsert = PreparedGraphVertexUpsert::prepare_with_trailing_actions(
            merge(),
            vec![
                expression(
                    A,
                    &[B],
                    &[
                        Op::ScalarColumn(0),
                        Op::Literal(Some(1)),
                        Op::Binary(Binary::Add),
                    ],
                ),
                expression(
                    B,
                    &[A],
                    &[
                        Op::ScalarColumn(0),
                        Op::Literal(Some(1)),
                        Op::Binary(Binary::Add),
                    ],
                ),
            ],
            vec![fail(COUNT)], // a valid but unselected erroneous branch
            vec![expression(
                COUNT,
                &[A, B],
                &[
                    Op::ScalarColumn(0),
                    Op::ScalarColumn(1),
                    Op::Binary(Binary::Add),
                ],
            )],
        )
        .unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_vertex_upsert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &upsert,
                policy(2_000_000),
                |_| -> Result<ElementId, ()> { panic!("matched vertex must not allocate") },
            )
            .await
            .unwrap();
        assert!(!outcome.created());
        assert_eq!(stats.action_effects, 3);
        assert!(stats.merge.evaluator.work_units > 0);
        assert!(
            matches!(completion, fgdb_types::EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(scalar(&db, A), Scalar::Int(10));
        assert_eq!(scalar(&db, B), Scalar::Int(6));
        assert_eq!(scalar(&db, COUNT), Scalar::Int(16));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn create_clause_reads_its_new_vertex_and_common_set_reads_that_clause() {
    let ((), report) = run_async_under_lab(0x5c73_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let upsert = PreparedGraphVertexUpsert::prepare_with_trailing_actions(
            merge(),
            vec![fail(COUNT)],
            vec![
                increment(COUNT),
                expression(
                    A,
                    &[COUNT],
                    &[Op::ScalarColumn(0), Op::Literal(Some(7)), Op::Coalesce],
                ),
            ],
            vec![expression(
                COUNT,
                &[COUNT],
                &[
                    Op::ScalarColumn(0),
                    Op::Literal(Some(4)),
                    Op::Binary(Binary::Add),
                ],
            )],
        )
        .unwrap();
        let mut allocated = 0;
        let (stats, outcome, _) = db
            .execute_graph_vertex_upsert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &upsert,
                policy(2_000_000),
                |_| {
                    allocated += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(1)))
                },
            )
            .await
            .unwrap();
        assert!(outcome.created());
        assert_eq!(allocated, 1);
        assert_eq!(stats.action_effects, 3);
        assert_eq!(scalar(&db, COUNT), Scalar::Int(5));
        assert_eq!(
            scalar(&db, A),
            Scalar::Int(7),
            "RHS must not see a sibling assignment"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn trailing_failure_restores_outer_prefix_but_not_an_issued_identity() {
    let ((), report) = run_async_under_lab(0x5c73_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![L], vec![(ID, Scalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let upsert = PreparedGraphVertexUpsert::prepare_with_trailing_actions(
            merge(),
            Vec::new(),
            vec![increment(COUNT)],
            vec![fail(COUNT)],
        )
        .unwrap();
        let mut allocated = 0;
        let result = transaction.execute_graph_vertex_upsert_governed(
            &mut db,
            &query,
            &upsert,
            policy(2_000_000),
            |_| {
                allocated += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(1)))
            },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphVertexUpsertError::Expression {
                clause: 1,
                action: 0,
                ..
            }))
        ));
        assert_eq!(allocated, 1);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(transaction.vertex(&db, VId(1)).unwrap().is_none());
        assert!(transaction.vertex(&db, VId(99)).unwrap().is_some());
        transaction.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_reads_and_both_clauses_share_the_original_evaluator_allowance() {
    let ((), report) = run_async_under_lab(0x5c73_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let upsert = PreparedGraphVertexUpsert::prepare_with_trailing_actions(
            merge(),
            vec![increment(A), increment(B)],
            Vec::new(),
            vec![increment(COUNT)],
        )
        .unwrap();
        let mut first = db.begin(&txcx).unwrap();
        let (stats, _) = first
            .execute_graph_vertex_upsert_governed(
                &mut db,
                &query,
                &upsert,
                policy(2_000_000),
                |_| -> Result<ElementId, ()> { panic!("unexpected allocation") },
            )
            .unwrap();
        first.abort();
        let mut second = db.begin(&txcx).unwrap();
        let result = second.execute_graph_vertex_upsert_governed(
            &mut db,
            &query,
            &upsert,
            policy(stats.merge.evaluator.work_units - 1),
            |_| -> Result<ElementId, ()> { panic!("unexpected allocation") },
        );
        assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
        assert_eq!(
            second.vertex(&db, VId(1)).unwrap().unwrap().props,
            db.vertex(VId(1)).unwrap().unwrap().props
        );
        second.abort();
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_expression_inputs_and_duplicate_fields_refuse_before_execution() {
    for action in [
        expression(A, &[], &[Op::ScalarColumn(0)]),
        expression(A, &[B], &[Op::ScalarColumn(1)]),
        expression(A, &[], &[Op::Local(0)]),
    ] {
        assert!(matches!(
            PreparedGraphVertexUpsert::prepare(merge(), vec![action], vec![]),
            Err(GraphVertexUpsertBuildError::ExpressionInputs { .. })
        ));
    }
    assert!(matches!(
        PreparedGraphVertexUpsert::prepare(merge(), vec![increment(A), increment(A)], vec![]),
        Err(GraphVertexUpsertBuildError::DuplicateProperty { .. })
    ));
    // A second clause may intentionally overwrite the first; its boundary and
    // source-property coordinates must both be represented in the transcript.
    let two_clauses = PreparedGraphVertexUpsert::prepare_with_trailing_actions(
        merge(),
        vec![increment(A)],
        vec![],
        vec![increment(A)],
    )
    .unwrap();
    let one_clause =
        PreparedGraphVertexUpsert::prepare(merge(), vec![increment(A)], vec![]).unwrap();
    assert_ne!(two_clauses.canonical_bytes(), one_clause.canonical_bytes());
    let a = PreparedGraphVertexUpsert::prepare(
        merge(),
        vec![expression(A, &[A], &[Op::ScalarColumn(0)])],
        vec![],
    )
    .unwrap();
    let b = PreparedGraphVertexUpsert::prepare(
        merge(),
        vec![expression(A, &[B], &[Op::ScalarColumn(0)])],
        vec![],
    )
    .unwrap();
    assert_ne!(a.canonical_bytes(), b.canonical_bytes());
}
