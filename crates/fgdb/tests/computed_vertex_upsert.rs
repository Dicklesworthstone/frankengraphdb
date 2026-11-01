//! Computed MERGE actions use the real native overlay, meter and completion.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaLimitDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerBinary as Binary,
    GraphIntegerExpression, GraphIntegerOp as Op, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertAction as Action, GraphVertexUpsertBuildError,
    GraphVertexUpsertError, GraphVertexUpsertPolicy, PreparedGraphVertexMerge,
    PreparedGraphVertexMergeText, PreparedGraphVertexUpsert, PreparedGraphVertexUpsertText,
};
use fgdb_types::{
    CanonicalDate, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalScalar as Scalar,
    CanonicalTime, CanonicalTimestamp, DatabaseSecurityNamespaceId, PurposeContexts, VId,
};

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
        (GraphSymbolKind::Property, "count") => Some(GraphSymbol::Property(COUNT)),
        (GraphSymbolKind::Property, "a") => Some(GraphSymbol::Property(A)),
        (GraphSymbolKind::Property, "b") => Some(GraphSymbol::Property(B)),
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
        .as_scalar()
        .expect("scalar property")
        .clone()
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

fn captured_instant() -> CanonicalTimestamp {
    CanonicalTimestamp::offset_only(1_437_511_232_142_000_000, 3600).unwrap()
}

#[test]
fn closed_temporal_values_run_only_in_the_selected_native_clause() {
    let ((), report) = run_async_under_lab(0x5c73_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let expected = [
            (
                "date()",
                Scalar::Date(CanonicalDate::parse_iso("2015-07-21").unwrap()),
            ),
            (
                "localtime()",
                Scalar::LocalTime(CanonicalLocalTime::parse_iso("21:40:32.142").unwrap()),
            ),
            (
                "time()",
                Scalar::Time(CanonicalTime::parse_iso("21:40:32.142+01:00").unwrap()),
            ),
            (
                "localdatetime()",
                Scalar::LocalDateTime(
                    CanonicalLocalDateTime::parse_iso("2015-07-21T21:40:32.142").unwrap(),
                ),
            ),
            ("datetime()", Scalar::Timestamp(captured_instant())),
            ("timestamp()", Scalar::Int(1_437_511_232_142)),
        ];
        let arguments = GqlParameters::new()
            .with_statement_time(captured_instant())
            .unwrap();
        for clause in 0..3 {
            for (value, expected) in &expected {
                let mut db = Database::open_memory(&contexts.commit(), keys())
                    .await
                    .unwrap();
                let matched = clause != 0;
                if matched {
                    db.write(&contexts.commit(), seed()).await.unwrap();
                }
                let actions = match clause {
                    0 => format!("ON CREATE SET n.a={value} ON MATCH SET n.b=date('invalid')"),
                    1 => format!("ON MATCH SET n.a={value} ON CREATE SET n.b=date('invalid')"),
                    _ => format!("ON CREATE SET n.b=1 ON MATCH SET n.b=2 SET n.a={value}"),
                };
                let text = format!("MERGE (n:Person {{id:1}}) {actions}");
                // Both erroneous unselected branches bind without evaluation.
                let upsert = PreparedGraphVertexUpsertText::prepare(&text, R, symbols)
                    .unwrap()
                    .bind_parameters(&arguments)
                    .unwrap();
                let before = db.frontier().unwrap();
                let mut transaction = db.begin(&contexts.txn()).unwrap();
                let mut allocations = 0;
                let (stats, outcome) = transaction
                    .execute_graph_vertex_upsert_governed(
                        &mut db,
                        &contexts.query(),
                        &upsert,
                        policy(2_000_000),
                        |_| {
                            allocations += 1;
                            Ok::<_, ()>(ElementId::Vertex(VId(1)))
                        },
                    )
                    .unwrap();
                assert_eq!(allocations, usize::from(!matched));
                assert_eq!(outcome.created(), !matched);
                assert_eq!(stats.action_effects, if clause == 2 { 2 } else { 1 });
                transaction
                    .finish(&mut db, &contexts.commit())
                    .await
                    .unwrap();
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                assert_eq!(scalar(&db, A), *expected, "{text}");
                assert_eq!(scalar(&db, ID), Scalar::Int(1));
                if matched {
                    assert_eq!(scalar(&db, COUNT), Scalar::Int(0));
                    assert_eq!(scalar(&db, B), Scalar::Int(if clause == 2 { 2 } else { 9 }));
                } else {
                    assert!(
                        db.vertex(VId(1))
                            .unwrap()
                            .unwrap()
                            .props
                            .iter()
                            .all(|(key, _)| *key != B)
                    );
                }
                assert_eq!(contexts.txn().outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn closed_temporal_rebinding_changes_context_without_refreshing_a_bound_action() {
    let ((), report) = run_async_under_lab(0x5c73_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let template = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) ON CREATE SET n.a=datetime(),n.b=timestamp() \
             ON MATCH SET n.a=datetime(),n.b=timestamp()",
            R,
            symbols,
        )
        .unwrap();
        let later = CanonicalTimestamp::offset_only(1_437_511_233_142_000_000, 3600).unwrap();
        let first = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_statement_time(captured_instant())
                    .unwrap(),
            )
            .unwrap();
        let second = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_statement_time(later.clone())
                    .unwrap(),
            )
            .unwrap();
        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
        assert_eq!(
            first.canonical_bytes(),
            template
                .bind_parameters(
                    &GqlParameters::new()
                        .with_statement_time(captured_instant())
                        .unwrap()
                )
                .unwrap()
                .canonical_bytes()
        );
        let mut allocations = 0;
        for (at, (upsert, timestamp, milliseconds)) in [
            (&first, captured_instant(), 1_437_511_232_142),
            (&first, captured_instant(), 1_437_511_232_142),
            (&second, later, 1_437_511_233_142),
        ]
        .into_iter()
        .enumerate()
        {
            let mut transaction = db.begin(&contexts.txn()).unwrap();
            let (_, outcome) = transaction
                .execute_graph_vertex_upsert_governed(
                    &mut db,
                    &contexts.query(),
                    upsert,
                    policy(2_000_000),
                    |_| {
                        allocations += 1;
                        Ok::<_, ()>(ElementId::Vertex(VId(1)))
                    },
                )
                .unwrap();
            assert_eq!(outcome.created(), at == 0);
            transaction
                .finish(&mut db, &contexts.commit())
                .await
                .unwrap();
            assert_eq!(scalar(&db, A), Scalar::Timestamp(timestamp));
            assert_eq!(scalar(&db, B), Scalar::Int(milliseconds));
        }
        assert_eq!(allocations, 1);
        assert_eq!(db.frontier().unwrap().0, 3);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_temporal_failure_restores_the_complete_existing_prefix() {
    let ((), report) = run_async_under_lab(0x5c73_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for matched in [false, true] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            if matched {
                db.write(&contexts.commit(), seed()).await.unwrap();
            }
            let before = db.frontier().unwrap();
            let original = db.vertex(VId(1)).unwrap();
            let mut transaction = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(99), vec![L], vec![(ID, Scalar::Int(99))]);
            transaction.write(&mut db, prefix).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let text = if matched {
                "MERGE (n:Person {id:1}) ON MATCH SET n.a=datetime() SET n.b=date('invalid')"
            } else {
                "MERGE (n:Person {id:1}) ON CREATE SET n.a=datetime() SET n.b=date('invalid')"
            };
            let upsert = PreparedGraphVertexUpsertText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(
                    &GqlParameters::new()
                        .with_statement_time(captured_instant())
                        .unwrap(),
                )
                .unwrap();
            let mut allocations = 0;
            let result = transaction.execute_graph_vertex_upsert_governed(
                &mut db,
                &contexts.query(),
                &upsert,
                policy(2_000_000),
                |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(1)))
                },
            );
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphVertexUpsertError::Expression {
                    clause: 1,
                    action: 0,
                    source: fgdb_gql::GraphIntegerError {
                        kind: fgdb_gql::GraphIntegerErrorKind::TemporalConstruction,
                        ..
                    },
                }))
            ));
            assert_eq!(allocations, usize::from(!matched));
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertex(&db, VId(1)).unwrap(), original);
            assert!(transaction.vertex(&db, VId(99)).unwrap().is_some());
            assert_eq!(db.frontier().unwrap(), before);
            transaction
                .finish(&mut db, &contexts.commit())
                .await
                .unwrap();
            assert_eq!(db.vertex(VId(1)).unwrap(), original);
            assert!(db.vertex(VId(99)).unwrap().is_some());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn closed_value_payload_has_exact_work_and_scratch_boundaries_without_prefix_leaks() {
    use fgdb_gql::GqlParameterType;
    use fgdb_gql::algebra::GraphValue;
    let ((), report) = run_async_under_lab(0x5c73_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let payload = "x".repeat(3 * 64 + 1);
        let text = format!(
            "MERGE (n:Person {{id:1}}) SET n.a=CASE WHEN true THEN '{payload}' ELSE '' END"
        );
        let upsert = PreparedGraphVertexUpsertText::prepare(&text, R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(matches!(
            upsert.after(),
            [Action::SetExpression { key: A, .. }]
        ));
        let mut measured = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![L], vec![(ID, Scalar::Int(99))]);
        measured.write(&mut db, prefix).unwrap();
        let (stats, _) = measured
            .execute_graph_vertex_upsert_governed(
                &mut db,
                &contexts.query(),
                &upsert,
                policy(2_000_000),
                |_| Err::<ElementId, ()>(()),
            )
            .unwrap();
        let vertex = measured.vertex(&db, VId(1)).unwrap().unwrap();
        assert_eq!(
            vertex
                .props
                .iter()
                .find(|(key, _)| *key == A)
                .unwrap()
                .1
                .as_scalar(),
            Some(&Scalar::ucs_basic_text(&payload).unwrap())
        );
        measured.abort();
        let legacy_usage = stats.merge.evaluator;
        let tiny = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) SET n.a=CASE WHEN true THEN 'x' ELSE '' END",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(matches!(
            tiny.after(),
            [Action::SetExpression { key: A, .. }]
        ));
        let mut measured = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![L], vec![(ID, Scalar::Int(99))]);
        measured.write(&mut db, prefix).unwrap();
        let (small, _) = measured
            .execute_graph_vertex_upsert_governed(
                &mut db,
                &contexts.query(),
                &tiny,
                policy(2_000_000),
                |_| Err::<ElementId, ()>(()),
            )
            .unwrap();
        let vertex = measured.vertex(&db, VId(1)).unwrap().unwrap();
        assert_eq!(
            vertex
                .props
                .iter()
                .find(|(key, _)| *key == A)
                .unwrap()
                .1
                .as_scalar(),
            Some(&Scalar::ucs_basic_text("x").unwrap())
        );
        measured.abort();
        // Preserve the original scalar CASEs as legacy positives. They do
        // not exercise SetValue's VM-to-stored conversion; their additional
        // payload quanta are admitted by the legacy staging owner.
        assert_eq!(
            legacy_usage.scratch_entries - small.merge.evaluator.scratch_entries,
            3
        );

        let legacy_graph = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) ON MATCH SET n.a=CASE WHEN false THEN n.id ELSE 1 END",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(matches!(legacy_graph.on_match(),
            [Action::SetExpression { key: A, properties, .. }] if properties == &[ID]));
        let mut measured = db.begin(&contexts.txn()).unwrap();
        measured
            .execute_graph_vertex_upsert_governed(
                &mut db,
                &contexts.query(),
                &legacy_graph,
                policy(2_000_000),
                |_| Err::<ElementId, ()>(()),
            )
            .unwrap();
        let vertex = measured.vertex(&db, VId(1)).unwrap().unwrap();
        assert_eq!(
            vertex
                .props
                .iter()
                .find(|(key, _)| *key == A)
                .unwrap()
                .1
                .as_scalar(),
            Some(&Scalar::Int(1))
        );
        measured.abort();

        // A declared map parameter field is a genuine closed value program,
        // rather than the scalar CASE compiler. Pin both native action owners
        // before measuring their complete execution allowances.
        let template = PreparedGraphVertexUpsertText::prepare_with_parameter_types(
            "MERGE (n:Person {id:1}) SET n.a=$arg.value",
            R,
            &[("arg", GqlParameterType::Map)],
            symbols,
        )
        .unwrap();
        let upsert = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_map(
                        "arg",
                        vec![(
                            "value".into(),
                            GraphValue::Scalar(Scalar::ucs_basic_text(&payload).unwrap()),
                        )],
                    )
                    .unwrap(),
            )
            .unwrap();
        let tiny = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_map(
                        "arg",
                        vec![(
                            "value".into(),
                            GraphValue::Scalar(Scalar::ucs_basic_text("x").unwrap()),
                        )],
                    )
                    .unwrap(),
            )
            .unwrap();
        assert!(matches!(upsert.after(), [Action::SetValue { key: A, .. }]));
        assert!(matches!(tiny.after(), [Action::SetValue { key: A, .. }]));
        assert_ne!(upsert.canonical_bytes(), tiny.canonical_bytes());
        let mut bills = Vec::new();
        for (upsert, expected) in [(&upsert, payload.as_str()), (&tiny, "x")] {
            let mut measured = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(99), vec![L], vec![(ID, Scalar::Int(99))]);
            measured.write(&mut db, prefix).unwrap();
            let (stats, outcome) = measured
                .execute_graph_vertex_upsert_governed(
                    &mut db,
                    &contexts.query(),
                    upsert,
                    policy(2_000_000),
                    |_| Err::<ElementId, ()>(()),
                )
                .unwrap();
            assert!(!outcome.created());
            assert_eq!(stats.action_effects, 1);
            let vertex = measured.vertex(&db, VId(1)).unwrap().unwrap();
            assert_eq!(
                vertex
                    .props
                    .iter()
                    .find(|(key, _)| *key == A)
                    .unwrap()
                    .1
                    .as_scalar(),
                Some(&Scalar::ucs_basic_text(expected).unwrap())
            );
            bills.push(stats.merge.evaluator);
            measured.abort();
        }
        let usage = bills[0];
        // Three extra 64-byte quanta cross three distinct owned copies:
        // map-field VM output (scratch), stored conversion (work + scratch),
        // and staged copy (scratch). Every scratch event also spends work.
        assert_eq!(usage.scratch_entries - bills[1].scratch_entries, 9);
        assert_eq!(usage.work_units - bills[1].work_units, 12);
        for (work, scratch, refused) in [
            (usage.work_units, usage.scratch_entries, None),
            (
                usage.work_units - 1,
                usage.scratch_entries,
                Some(GlaLimitDimension::WorkUnits),
            ),
            (
                usage.work_units,
                usage.scratch_entries - 1,
                Some(GlaLimitDimension::ScratchEntries),
            ),
        ] {
            let mut transaction = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(99), vec![L], vec![(ID, Scalar::Int(99))]);
            transaction.write(&mut db, prefix).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let mut exact = policy(work);
            exact.merge.query.evaluator.max_scratch_entries = scratch;
            let result = transaction.execute_graph_vertex_upsert_governed(
                &mut db,
                &contexts.query(),
                &upsert,
                exact,
                |_| Err::<ElementId, ()>(()),
            );
            if let Some(dimension) = refused {
                let limit = if dimension == GlaLimitDimension::WorkUnits {
                    work
                } else {
                    scratch
                };
                assert!(matches!(result, Err(GqlQueryError::Evaluator(error))
                    if error.dimension == dimension && error.limit == limit && error.observed == u128::from(limit) + 1));
                assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
                assert_eq!(
                    transaction.vertex(&db, VId(1)).unwrap(),
                    db.vertex(VId(1)).unwrap()
                );
            } else {
                let (actual, outcome) = result.unwrap();
                assert_eq!(actual.merge.evaluator, usage);
                assert!(!outcome.created());
                let vertex = transaction.vertex(&db, VId(1)).unwrap().unwrap();
                assert_eq!(
                    vertex
                        .props
                        .iter()
                        .find(|(key, _)| *key == A)
                        .unwrap()
                        .1
                        .as_scalar(),
                    Some(&Scalar::ucs_basic_text(&payload).unwrap())
                );
            }
            assert!(transaction.vertex(&db, VId(99)).unwrap().is_some());
            transaction.abort();
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(scalar(&db, A), Scalar::Int(5));
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn closed_upsert_actions_reject_random_clock_and_ambient_row_programs() {
    use fgdb_gql::{GraphVertexMergeRowAction, GraphWriteChainStep, PreparedGraphWriteChainText};
    let arguments = GqlParameters::new()
        .with_statement_time(captured_instant())
        .unwrap();
    for value in [
        "rand()",
        "date.realtime(date())",
        "CASE WHEN true THEN 1 ELSE rand() END",
    ] {
        let text = format!("MERGE (n:Person {{id:1}}) ON MATCH SET n.a={value}");
        let template = PreparedGraphVertexUpsertText::prepare(&text, R, symbols).unwrap();
        assert!(matches!(
            template.bind_parameters(&arguments),
            Err(fgdb_gql::GraphVertexUpsertTextError {
                kind: fgdb_gql::GraphVertexUpsertTextErrorKind::UpsertBuild(
                    GraphVertexUpsertBuildError::ExpressionInputs { action: 0, .. }
                ),
                ..
            })
        ));
    }
    let chain = PreparedGraphWriteChainText::prepare(
        "UNWIND [1] AS row MERGE (n:Person {id:1}) ON MATCH SET n.a=row RETURN n",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let [GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
        unreachable!()
    };
    let [GraphVertexMergeRowAction::Value { value, .. }] =
        &query.merge().actions().unwrap().on_match[..]
    else {
        unreachable!()
    };
    assert!(matches!(
        PreparedGraphVertexUpsert::prepare(
            merge(),
            vec![Action::SetValue {
                key: A,
                value: value.clone(),
            }],
            vec![]
        ),
        Err(GraphVertexUpsertBuildError::ExpressionInputs { action: 0, .. })
    ));
    let legacy = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {id:1}) ON MATCH SET n.a=CASE WHEN false THEN n.id ELSE 1 END",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert!(matches!(legacy.on_match(),
        [Action::SetExpression { key: A, properties, .. }] if properties == &[ID]));
    assert!(matches!(
        PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) ON MATCH SET n.a=\
             {value:CASE WHEN false THEN n.id ELSE 1 END}.value",
            R,
            symbols,
        ),
        Err(fgdb_gql::GraphVertexUpsertTextError {
            kind: fgdb_gql::GraphVertexUpsertTextErrorKind::Query(
                fgdb_gql::GraphPatternTextErrorKind::Expected("the MERGE vertex variable")
            ),
            ..
        })
    ));
    for (value, expression) in [
        ("7", false),
        ("-2", false),
        ("true", false),
        ("null", false),
        ("'literal'", false),
        ("$value", false),
        ("n.a+1", true),
    ] {
        let text = format!("MERGE (n:Person {{id:1}}) SET n.a={value}");
        let parameters = if value == "$value" {
            GqlParameters::new().with_int64("value", 7).unwrap()
        } else {
            GqlParameters::new()
        };
        let upsert = PreparedGraphVertexUpsertText::prepare(&text, R, symbols)
            .unwrap()
            .bind_parameters(&parameters)
            .unwrap();
        if expression {
            assert!(matches!(
                upsert.after(),
                [Action::SetExpression { key: A, .. }]
            ));
        } else {
            assert!(matches!(
                upsert.after(),
                [Action::SetProperty { key: A, .. }]
            ));
        }
    }
}
