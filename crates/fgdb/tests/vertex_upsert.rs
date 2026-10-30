//! Branch-specific unique vertex MERGE actions over the real transaction overlay.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertVertex, PreparedGraphInsert};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphMutationValue,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    GraphVertexUpsertAction, GraphVertexUpsertBranch, GraphVertexUpsertError,
    GraphVertexUpsertPolicy, PreparedGraphText, PreparedGraphVertexMerge,
    PreparedGraphVertexUpsert, PreparedGraphVertexUpsertText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const OLD: PropertyKeyId = PropertyKeyId(3);
const MARKED: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "old") => Some(GraphSymbol::Property(OLD)),
        _ => None,
    }
}
fn scalar(value: i64) -> GqlScalarParameter {
    GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap()
}
fn upsert(value: i64) -> PreparedGraphVertexUpsert {
    let text = format!("MATCH (n) WHERE n.p={value} RETURN ALL n");
    let selection = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let create = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![(P, GraphMutationValue::Literal(scalar(value)))],
        }],
        vec![],
    )
    .unwrap();
    let merge = PreparedGraphVertexMerge::prepare(selection, R, 0, create).unwrap();
    PreparedGraphVertexUpsert::prepare(
        merge,
        vec![
            GraphVertexUpsertAction::SetProperty {
                key: Q,
                value: scalar(100),
            },
            GraphVertexUpsertAction::SetLabel {
                label: MARKED,
                present: true,
            },
        ],
        vec![
            GraphVertexUpsertAction::SetProperty {
                key: Q,
                value: scalar(200),
            },
            GraphVertexUpsertAction::SetLabel {
                label: MARKED,
                present: true,
            },
        ],
    )
    .unwrap()
}
fn policy(max_actions: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000)),
        max_actions,
    )
}

fn replacement(text: &str) -> PreparedGraphVertexUpsert {
    PreparedGraphVertexUpsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

#[test]
fn whole_map_replacement_freezes_each_clause_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x6e29_3010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![MARKED],
            vec![
                (P, CanonicalScalar::Int(7)),
                (Q, CanonicalScalar::Int(2)),
                (OLD, CanonicalScalar::Int(99)),
            ],
        );
        db.write(&commit, seed).await.unwrap();
        let upsert =
            replacement("MERGE (n {p:7}) ON MATCH SET n = {p:n.p,q:n.q+1} SET n = {q:n.q+1}");
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_upsert_governed(&mut db, &query, &upsert, policy(5), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(90)))
            })
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert_eq!(stats.action_effects, 5);
        assert_eq!(stats.property_removals, 2);
        let expected = vec![(Q, CanonicalScalar::Int(4).into())];
        assert_eq!(txn.vertex(&db, VId(1)).unwrap().unwrap().props, expected);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let row = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(row.props, expected);
        assert_eq!(row.labels, vec![MARKED]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replacement_counts_every_removed_field_and_rolls_back_late_refusals() {
    let ((), report) = run_async_under_lab(0x6e29_3011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=2 {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![
                    (P, CanonicalScalar::Int(7)),
                    (Q, CanonicalScalar::Int(2)),
                    (OLD, CanonicalScalar::Int(3)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let original = db.vertices().unwrap();
        let upsert = replacement("MERGE (n {p:7}) ON MATCH SET n = {p:7}");
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let error =
            txn.execute_graph_vertex_upserts_governed(&mut db, &query, &upsert, policy(5), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(90)))
            });
        assert!(matches!(
            error,
            Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: 5,
                observed: 6,
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert_eq!(db.vertices().unwrap(), original);
        let late = replacement("MERGE (n {p:7}) ON MATCH SET n = {p:7} SET n.q=1/(n.p-7)");
        assert!(matches!(
            txn.execute_graph_vertex_upserts_governed(
                &mut db,
                &query,
                &late,
                policy(100),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(90))),
            ),
            Err(GqlQueryError::Source(GraphVertexUpsertError::Expression {
                clause: 1,
                ..
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        let (stats, outcomes) = txn
            .execute_graph_vertex_upserts_governed(&mut db, &query, &upsert, policy(6), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(90)))
            })
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!((stats.action_effects, stats.property_removals), (6, 4));
        txn.commit(&mut db, &commit).await.unwrap();
        for row in db.vertices().unwrap() {
            assert_eq!(row.props, vec![(P, CanonicalScalar::Int(7).into())]);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn map_parameters_and_row_merges_replace_through_the_public_write_path() {
    use fgdb_gql::{GraphWriteProgramPolicy, algebra::GraphValue};

    let ((), report) = run_async_under_lab(0x6e29_3012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=2 {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![
                    (P, CanonicalScalar::Int(id as i64)),
                    (OLD, CanonicalScalar::Int(9)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let row_statement =
            "UNWIND [1,2] AS id MERGE (n {p:id}) ON MATCH SET n = {p:id,q:id*10} RETURN n.p,n.q";
        assert!(
            db.query_write(
                &txcx,
                &query,
                &commit,
                row_statement,
                &GqlParameters::new(),
                symbols,
                R,
                GraphWriteProgramPolicy::new(policy(100).merge.query, 5, 10, 0),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(90))),
            )
            .await
            .is_err()
        );
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        let allowance = GraphWriteProgramPolicy::new(policy(100).merge.query, 100, 10, 0);
        db.query_write(
            &txcx,
            &query,
            &commit,
            row_statement,
            &GqlParameters::new(),
            symbols,
            R,
            allowance,
            |_| Ok::<_, ()>(ElementId::Vertex(VId(90))),
        )
        .await
        .unwrap();
        for id in 1..=2 {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(id as i64).into()),
                    (Q, CanonicalScalar::Int(id as i64 * 10).into()),
                ]
            );
        }
        let arguments = GqlParameters::new()
            .with_map(
                "props",
                vec![("q".into(), GraphValue::Scalar(CanonicalScalar::Int(42)))],
            )
            .unwrap();
        db.query_write(
            &txcx,
            &query,
            &commit,
            "MERGE (n {p:1}) ON MATCH SET n = $props RETURN n.q",
            &arguments,
            symbols,
            R,
            allowance,
            |_| Ok::<_, ()>(ElementId::Vertex(VId(90))),
        )
        .await
        .unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(42).into())]
        );
        // NULL in a replacement is an unset, never a stored null property.
        let null_map = GqlParameters::new()
            .with_map(
                "props",
                vec![("q".into(), GraphValue::Scalar(CanonicalScalar::Null))],
            )
            .unwrap();
        db.query_write(
            &txcx,
            &query,
            &commit,
            "MERGE (n {p:2}) SET n = $props RETURN n.q",
            &null_map,
            symbols,
            R,
            allowance,
            |_| Ok::<_, ()>(ElementId::Vertex(VId(90))),
        )
        .await
        .unwrap();
        assert!(db.vertex(VId(2)).unwrap().unwrap().props.is_empty());
        // A common SET follows ON CREATE too, observing that branch's values.
        db.query_write(
            &txcx,
            &query,
            &commit,
            "MERGE (n {p:3}) ON CREATE SET n.q=2 SET n = {q:n.q+1} RETURN n.q",
            &GqlParameters::new(),
            symbols,
            R,
            allowance,
            |_| Ok::<_, ()>(ElementId::Vertex(VId(3))),
        )
        .await
        .unwrap();
        assert_eq!(
            db.vertex(VId(3)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(3).into())]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn on_match_updates_existing_vertex_without_allocating() {
    let ((), report) = run_async_under_lab(0x6e29_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();

        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_upsert_governed(
                &mut db,
                &query,
                &upsert(7),
                policy(2),
                |_| -> Result<ElementId, ()> { panic!("ON MATCH must not allocate") },
            )
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Match);
        assert_eq!(stats.action_effects, 2);
        let staged = txn.vertex(&db, VId(1)).unwrap().unwrap();
        assert_eq!(staged.labels, vec![MARKED]);
        assert_eq!(
            staged.props,
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Int(100).into())
            ]
        );
        txn.commit(&mut db, &commit).await.unwrap();
        let row = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(row.labels, vec![MARKED]);
        assert_eq!(
            row.props,
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Int(100).into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn on_create_decorates_new_vertex_atomically_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x6e29_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_upsert_governed(&mut db, &query, &upsert(9), policy(2), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(9)))
            })
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(9)));
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Create);
        assert_eq!(stats.action_effects, 2);
        let staged = txn.vertex(&db, VId(9)).unwrap().unwrap();
        assert_eq!(staged.labels, vec![MARKED]);
        assert_eq!(
            staged.props,
            vec![
                (P, CanonicalScalar::Int(9).into()),
                (Q, CanonicalScalar::Int(200).into())
            ]
        );
        assert!(db.vertex(VId(9)).unwrap().is_none());
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let row = db.vertex(VId(9)).unwrap().unwrap();
        assert_eq!(row.labels, vec![MARKED]);
        assert_eq!(
            row.props,
            vec![
                (P, CanonicalScalar::Int(9).into()),
                (Q, CanonicalScalar::Int(200).into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn create_branch_action_limit_refusal_rolls_back_created_vertex_but_not_allocator() {
    let ((), report) = run_async_under_lab(0x6e29_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result = txn.execute_graph_vertex_upsert_governed(
            &mut db,
            &query,
            &upsert(11),
            policy(1),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(11))),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: 1,
                observed: 2
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(11)).unwrap().is_none());
        assert!(db.vertex(VId(11)).unwrap().is_none());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ambiguity_refuses_before_branch_actions_and_preserves_workspace() {
    let ((), report) = run_async_under_lab(0x6e29_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        assert!(
            txn.execute_graph_vertex_upsert_governed(
                &mut db,
                &query,
                &upsert(7),
                policy(2),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(99))),
            )
            .is_err()
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(
            txn.vertex(&db, VId(1))
                .unwrap()
                .unwrap()
                .props
                .iter()
                .all(|(key, _)| *key != Q)
        );
        assert!(
            txn.vertex(&db, VId(2))
                .unwrap()
                .unwrap()
                .props
                .iter()
                .all(|(key, _)| *key != Q)
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
