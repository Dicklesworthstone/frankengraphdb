//! Exercise UNWIND through public native text writers, not the explicit adapter.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const COUNTER: &str = "UNWIND $rows AS row MERGE (n:Person {p:row.p}) \
    ON CREATE SET n.q=0 SET n.q=n.q+row.q";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy(effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        effects,
        vertices,
        0,
    )
}
fn row(p: i64, q: CanonicalScalar) -> GraphValue {
    GraphValue::map(vec![
        ("p".into(), GraphValue::Scalar(CanonicalScalar::Int(p))),
        ("q".into(), GraphValue::Scalar(q)),
    ])
    .unwrap()
}
fn rows(values: &[(i64, i64)]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            values
                .iter()
                .map(|&(p, q)| row(p, CanonicalScalar::Int(q)))
                .collect(),
        )
        .unwrap()
}

#[test]
fn native_engine_upserts_repeated_keys_once_and_reopens() {
    let ((), report) = run_async_under_lab(0x554e_5701, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let arguments = rows(&[(1, 2), (2, 4), (1, 3)]);
        let frozen = arguments.canonical_bytes();
        let result = db
            .query_write_engine(
                &txcx,
                &query,
                &commit,
                COUNTER,
                &arguments,
                symbols,
                R,
                policy(10, 2),
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion: Some(completion),
        } = result
        else {
            panic!("native no-RETURN UNWIND must return its one completed receipt")
        };
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        let first = receipt.steps()[0].merged_vertex().unwrap().vertex();
        let second = receipt.steps()[1].merged_vertex().unwrap().vertex();
        assert_eq!(receipt.steps()[2].merged_vertex().unwrap().vertex(), first);
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(
            db.vertex(first).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(5).into())
            ]
        );
        assert_eq!(
            db.vertex(second).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(2).into()),
                (Q, CanonicalScalar::Int(4).into())
            ]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_native_updates_see_staged_rows_and_do_not_publish() {
    let ((), report) = run_async_under_lab(0x554e_5702, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(
            VId(90),
            vec![L],
            vec![(P, CanonicalScalar::Int(90)), (Q, CanonicalScalar::Int(7))],
        );
        txn.write(&mut db, prefix).unwrap();
        let result = txn
            .query_write_engine(
                &mut db,
                &query,
                "UNWIND $rows AS row MATCH (n:Person {p:row.p}) SET n.q=n.q+row.q",
                &rows(&[(90, 2), (90, 3)]),
                symbols,
                R,
                policy(2, 0),
            )
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion: None,
        } = result
        else {
            panic!("an outer transaction must retain completion ownership")
        };
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(90)).unwrap().is_none());
        assert_eq!(
            txn.vertex(&db, VId(90)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(90).into()),
                (Q, CanonicalScalar::Int(12).into())
            ]
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_late_type_refusal_calls_neither_catalog_nor_allocator() {
    let ((), report) = run_async_under_lab(0x554e_5703, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let calls = AtomicUsize::new(0);
        let allocations = AtomicUsize::new(0);
        let arguments = GqlParameters::new()
            .with_list(
                "rows",
                vec![
                    row(1, CanonicalScalar::Int(2)),
                    row(2, CanonicalScalar::Bool(true)),
                ],
            )
            .unwrap();
        let result = db
            .query_write(
                &txcx,
                &query,
                &commit,
                COUNTER,
                &arguments,
                |kind, name: &str| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    symbols(kind, name)
                },
                R,
                policy(10, 2),
                |_| {
                    allocations.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(ElementId::Vertex(VId(1)))
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
                row: 1,
                kind: GraphUnwindRowError::IncompatibleFieldTypes,
                ..
            }))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(allocations.load(Ordering::Relaxed), 0);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_late_overflow_retains_row_location_and_outer_prefix() {
    let ((), report) = run_async_under_lab(0x554e_5704, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(
            VId(90),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(90)),
                (Q, CanonicalScalar::Int(i64::MAX)),
            ],
        );
        txn.write(&mut db, prefix).unwrap();
        let result = txn.query_write(
            &mut db,
            &query,
            COUNTER,
            &rows(&[(1, 2), (90, 1)]),
            symbols,
            R,
            policy(10, 1),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
        );
        let Err(QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location),
            source,
        })) = result
        else {
            panic!("expected a located native UNWIND failure")
        };
        assert_eq!(location.argument_set, 1);
        assert_eq!(location.statement, 0);
        assert_eq!(location.span, 0..COUNTER.len());
        assert!(matches!(
            source,
            fgdb_gql::GraphWriteProgramError::VertexUpsert { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
        assert_eq!(
            txn.vertex(&db, VId(90)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(90).into()),
                (Q, CanonicalScalar::Int(i64::MAX).into())
            ]
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_unwind_creation_quota_is_shared_and_failed_batch_publishes_nothing() {
    let ((), report) = run_async_under_lab(0x554e_5705, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let calls = AtomicUsize::new(0);
        let result = db
            .query_write(
                &txcx,
                &query,
                &commit,
                COUNTER,
                &rows(&[(1, 2), (2, 3)]),
                symbols,
                R,
                policy(10, 1),
                |_| {
                    let at = calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(ElementId::Vertex(VId(at as u128 + 1)))
                },
            )
            .await;
        assert!(matches!(result, Err(QueryWriteError::Execute(
            GraphWriteScriptExecutionError::BatchProgram { location: Some(location), .. }
        )) if location.argument_set == 1));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "second row cannot obtain a fresh creation quota"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_create_return_is_not_intercepted_and_script_bind_errors_keep_their_arm() {
    let ((), report) = run_async_under_lab(0x554e_5706, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let result = db
            .query_write_engine(
                &txcx,
                &query,
                &commit,
                "UNWIND $rows AS row CREATE (n:Person {p:row.p}) RETURN n.p AS p",
                &rows(&[(1, 2), (2, 3)]),
                symbols,
                R,
                policy(0, 2),
            )
            .await
            .unwrap();
        assert!(matches!(result, QueryResult::Rows { columns, rows }
            if columns == vec!["p".to_owned()] && rows.len() == 2));
        let before = db.frontier().unwrap();
        let result = db
            .query_write_engine(
                &txcx,
                &query,
                &commit,
                "CREATE (n:Person {p:$missing})",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 1),
            )
            .await;
        assert!(matches!(
            result,
            Err(QueryWriteError::Execute(
                GraphWriteScriptExecutionError::Binding(_)
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn five_thousand_native_unwind_rows_commit_once_and_reopen_without_chunking() {
    let ((), report) = run_async_under_lab(0x554e_5707, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let arguments = rows(&(0..5_000).map(|id| (id, id * 2)).collect::<Vec<_>>());
        assert!(arguments.canonical_byte_len() > fgdb_gql::algebra::MAX_SCALAR_PREDICATE_BYTES);
        let frozen = arguments.canonical_bytes();
        let result = db
            .query_write_engine(
                &txcx,
                &query,
                &commit,
                "UNWIND $rows AS row CREATE (:Person {p:row.p,q:row.q})",
                &arguments,
                symbols,
                R,
                policy(0, 5_000),
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion: Some(completion),
        } = result
        else {
            panic!("native CREATE owns one completed statement")
        };
        assert_eq!(receipt.stats().completed_statements, 1);
        assert_eq!(receipt.stats().created_vertices, 5_000);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq }
                if commit_seq.0 == before.0 + 1
        ));
        assert_eq!(db.delta_since(before).unwrap().count(), 1);
        assert_eq!(arguments.canonical_bytes(), frozen);
        let vertices = db.vertices().unwrap();
        assert_eq!(vertices.len(), 5_000);
        let mut observed = vertices
            .iter()
            .map(|vertex| {
                assert_eq!(vertex.labels, vec![L]);
                let [
                    (P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(p))),
                    (Q, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(q))),
                ] = vertex.props.as_slice()
                else {
                    panic!("both row fields must survive native storage preparation")
                };
                (*p, *q)
            })
            .collect::<Vec<_>>();
        observed.sort_unstable();
        assert_eq!(
            observed,
            (0..5_000).map(|id| (id, id * 2)).collect::<Vec<_>>()
        );
        assert_eq!(txcx.outstanding_obligations(), 0);

        // The transport allowance must not reset or bypass the one-statement
        // creation quota. Refusing the last occurrence publishes no prefix.
        let committed = db.frontier().unwrap();
        assert!(
            db.query_write_engine(
                &txcx,
                &query,
                &commit,
                "UNWIND $rows AS row CREATE (:Person {p:row.p,q:row.q})",
                &arguments,
                symbols,
                R,
                policy(0, 4_999),
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), committed);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap(), committed);
        assert_eq!(reopened.vertices().unwrap(), vertices);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
