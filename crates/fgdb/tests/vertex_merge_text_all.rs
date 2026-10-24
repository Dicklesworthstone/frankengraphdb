//! Public text dispatch must reach all-target vertex MERGE, not the deliberately
//! unique typed primitive. These tests use native transactions and real storage;
//! expected values are enumerated independently of the result executor.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeError, GraphVertexUpsertError, GraphWriteIdentityRequest,
    GraphWriteProgramPolicy, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const TARGETS: [VId; 3] = [VId(1), VId((1_u128 << 96) + 2), VId((1_u128 << 112) + 3)];
const OUTSIDE: VId = VId(90);
const PREFIX_VERTEX: VId = VId(91);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(rows: u64, actions: u64, creates: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 5_000_000, 2_000_000),
        actions,
        creates,
        0,
    )
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn values(result: QueryResult, names: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("a returning write must not become a no-result script")
    };
    assert_eq!(
        columns,
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("write RETURN supplies native row values"),
                })
                .collect()
        })
        .collect()
}

fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("existing MERGE targets must not allocate")
}

async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (vertex, p) in TARGETS.into_iter().zip([1, 2, 3]) {
        batch.create_vertex(
            vertex,
            vec![ITEM],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(7))],
        );
    }
    batch.create_vertex(
        OUTSIDE,
        vec![ITEM],
        vec![
            (P, CanonicalScalar::Int(100)),
            (Q, CanonicalScalar::Int(99)),
        ],
    );
    db.write(commit, batch).await.unwrap();
}

fn assert_targets(db: &Database<MemVfs>, ps: [i64; 3], q: i64) {
    for (vertex, p) in TARGETS.into_iter().zip(ps) {
        assert_eq!(
            db.vertex(vertex).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(p).into()),
                (Q, CanonicalScalar::Int(q).into())
            ]
        );
    }
    assert_eq!(
        db.vertex(OUTSIDE).unwrap().unwrap().props,
        vec![
            (P, CanonicalScalar::Int(100).into()),
            (Q, CanonicalScalar::Int(99).into())
        ]
    );
}

#[test]
fn transaction_text_updates_all_targets_while_explicit_unique_api_stays_unique() {
    let ((), report) = run_async_under_lab(0x7e87_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let unique = PreparedGraphVertexUpsertQueryText::prepare(
            "MERGE (n:Item {q:7}) RETURN n",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let error = transaction
            .execute_graph_vertex_upsert_query_governed(
                &mut db,
                &cx,
                &unique,
                policy(10, 9, 0).vertex_upsert_policy(),
                |_| -> Result<ElementId, ()> { panic!("unique match must not create") },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexUpsertError::Merge(
                GraphVertexMergeError::AmbiguousMatches { observed: 3 }
            ))
        ));

        let params = GqlParameters::new()
            .with_int64("key", 7)
            .unwrap()
            .with_int64("step", 10)
            .unwrap();
        let result = transaction
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:Item {q:$key}) ON MATCH SET n.p=n.p+$step,n.q=8 \
                 SET n.p=n.p*2 WITH n.p AS p RETURN count(*) AS targets,sum(p) AS total",
                &params,
                symbols,
                R,
                policy(1, 9, 0),
                no_allocation,
            )
            .unwrap();
        assert_eq!(
            values(result, &["targets", "total"]),
            vec![vec![int(3), int(72)]]
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_targets(&db, [1, 2, 3], 7);
        // Every original target changes even though earlier actions change the
        // MERGE identity key. No target is lost by re-matching mid-statement.
        for (vertex, p) in TARGETS.into_iter().zip([22, 24, 26]) {
            assert_eq!(
                transaction.vertex(&db, vertex).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(8).into())
                ]
            );
        }
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_targets(&db, [22, 24, 26], 8);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn autocommit_updates_all_targets_before_paging_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x7e87_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MERGE (n:Item {q:7}) ON MATCH SET n.p=n.p+10 \
                 RETURN n AS vertex,n.p AS p ORDER BY p DESC SKIP 1 LIMIT 1",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 3, 0),
                no_allocation,
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["vertex", "p"]),
            vec![vec![GraphValue::Vertex(TARGETS[1]), int(12)]]
        );
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_targets(&db, [11, 12, 13], 7);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_targets(&db, [11, 12, 13], 7);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_creation_commits_once_and_later_match_preserves_allocator_coordinates() {
    let ((), report) = run_async_under_lab(0x7e87_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let allocations = AtomicUsize::new(0);
        let created = VId((1_u128 << 120) + 7);
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MERGE (n:Item {q:7}) ON CREATE SET n.p=4 ON MATCH SET n.p=n.p+1 \
                 SET n.p=n.p*2 WITH n.p AS p WHERE p < 0 RETURN p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 2, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    allocations.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(ElementId::Vertex(created))
                },
            )
            .await
            .unwrap();
        assert!(values(result, &["p"]).is_empty());
        assert_eq!(allocations.load(Ordering::Relaxed), 1);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(
            db.vertex(created).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(8).into()),
                (Q, CanonicalScalar::Int(7).into())
            ]
        );
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MERGE (n:Item {q:7}) ON CREATE SET n.p=4 ON MATCH SET n.p=n.p+1 \
                 SET n.p=n.p*2 RETURN n AS vertex,n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 2, 0),
                no_allocation,
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["vertex", "p"]),
            vec![vec![GraphValue::Vertex(created), int(18)]]
        );
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(db.frontier().unwrap().0, before.0 + 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_return_errors_rollback_all_targets_and_preserve_the_transaction_prefix() {
    let ((), report) = run_async_under_lab(0x7e87_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let text = "MERGE (n:Item {q:7}) ON MATCH SET n.p=n.p-3 \
                    WITH n.p AS p RETURN 12/p AS quotient LIMIT 0";
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(PREFIX_VERTEX, vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let error = transaction
            .query_write(
                &mut db,
                &cx,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 3, 0),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::Returning(_)
            ))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), before);
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_targets(&db, [1, 2, 3], 7);
        assert!(db.vertex(PREFIX_VERTEX).unwrap().is_some());
        let before = db.frontier().unwrap();
        let error = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 3, 0),
                no_allocation,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::Returning(_)
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_targets(&db, [1, 2, 3], 7);
        assert!(db.vertex(PREFIX_VERTEX).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_action_caps_are_statement_wide_and_result_caps_apply_after_all_updates() {
    let ((), report) = run_async_under_lab(0x7e87_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(PREFIX_VERTEX, vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let text = "MERGE (n:Item {q:7}) ON MATCH SET n.p=n.p+10 SET n.q=8 RETURN n.p AS p";
        let budget = policy(3, 5, 0);
        assert_eq!(budget.vertex_upsert_policy().max_actions, 5);
        let error = transaction
            .query_write(
                &mut db,
                &cx,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                budget,
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::ActionLimit {
                    limit: 5,
                    observed: 6
                }
            ))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        let error = transaction
            .query_write(
                &mut db,
                &cx,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(2, 6, 0),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::VertexUpsert(GqlQueryError::Rows(_))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        let result = transaction
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:Item {q:7}) ON MATCH SET n.p=n.p+10 SET n.q=8 \
                 WITH n.p AS p WHERE p < 0 RETURN p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 6, 0),
                no_allocation,
            )
            .unwrap();
        assert!(values(result, &["p"]).is_empty());
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_targets(&db, [11, 12, 13], 8);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
