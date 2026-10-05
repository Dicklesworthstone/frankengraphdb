//! Embedded UNWIND writes use one admitted, rollback-guarded native program.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::unwind_write::GraphUnwindWriteText;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const UPSERT: &str = "UNWIND $rows AS row MERGE (n:Person {p:row.p}) SET n.q=row.q";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(20_000, 20_000, 2_000_000, 2_000_000),
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
fn repeated_keys_commit_once_and_survive_reopen() {
    let ((), report) = run_async_under_lab(0x5c72_0001, |root| async move {
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
        let definition = GraphUnwindWriteText::parse(UPSERT).unwrap();
        let arguments = rows(&[(1, 10), (2, 20), (1, 30)]);
        let frozen = arguments.canonical_bytes();
        let calls = AtomicUsize::new(0);
        let (receipt, completion) = db
            .execute_graph_unwind_write_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                &arguments,
                R,
                3,
                symbols,
                policy(3, 2),
                |request| {
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    let ordinal = calls.fetch_add(1, Ordering::Relaxed);
                    let vid = match ordinal {
                        0 => {
                            assert_eq!(request.statement, 0);
                            VId(10)
                        }
                        1 => {
                            assert_eq!(request.statement, 1);
                            VId(11)
                        }
                        _ => panic!("a repeated key must not allocate another vertex"),
                    };
                    Ok::<_, ()>(ElementId::Vertex(vid))
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(
            db.vertex(VId(10)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(30))]
        );
        assert_eq!(
            db.vertex(VId(11)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(20))]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_entry_stages_without_publishing_and_sees_its_outer_prefix() {
    let ((), report) = run_async_under_lab(0x5c72_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(10), vec![PERSON], vec![(P, CanonicalScalar::Int(1))]);
        transaction.write(&mut db, prefix).unwrap();
        let definition = GraphUnwindWriteText::parse(UPSERT).unwrap();
        let calls = AtomicUsize::new(0);
        let receipt = transaction
            .execute_graph_unwind_write_governed(
                &mut db,
                &query,
                &definition,
                &rows(&[(1, 10), (2, 20), (1, 30)]),
                R,
                3,
                symbols,
                policy(3, 1),
                |request| {
                    assert_eq!(request.statement, 1);
                    assert_eq!(calls.fetch_add(1, Ordering::Relaxed), 0);
                    Ok::<_, ()>(ElementId::Vertex(VId(11)))
                },
            )
            .unwrap();
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(
            transaction.vertex(&db, VId(10)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(30))]
        );
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
