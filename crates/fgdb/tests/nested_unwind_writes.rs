//! Nested JSON fields enter ordinary native binding, staged writes and durable
//! publication. Storage reads below are independent of the document selector.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryWriteError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId};
use std::convert::Infallible;

const R: RelationId = RelationId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const QUERY: &str = "UNWIND $rows AS row MERGE (n:Entity {id:row.identity.id}) \
    SET n.name=row.versions[-1].name";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x25; 32], DatabaseSecurityNamespaceId([0x26; 32]), [0x27; 32])
}
fn read_policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000) }
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(read_policy(), 10_000, 10_000, 10_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(PropertyKeyId(3))),
        _ => None,
    }
}
fn object(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(key, value)| (key.into(), value)).collect()).unwrap()
}
fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn list(values: Vec<GraphValue>) -> GraphValue { GraphValue::List(values.into_boxed_slice()) }
fn document(id: i64, name: &str) -> GraphValue {
    object(vec![("identity", object(vec![("id", int(id))])),
        ("versions", list(vec![object(vec![("name", GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(name).unwrap()))])]))])
}
fn args(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}

#[test]
fn nested_bulk_upserts_and_match_updates_publish_once_and_survive_reopen() {
    let ((), report) = run_async_under_lab(0x6e65_7301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &dir, keys()).await.unwrap();
        let mut next = 100_u128;
        let arguments = args(vec![document(1, "first"), document(2, "second"),
            document(1, "updated\n'quote'; $aa")]);
        let result = db.query_write(&txcx, &cx, &commit, QUERY, &arguments, symbols, R, policy(), |_| {
            next += 1; Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
        }).await.unwrap();
        assert!(matches!(result, QueryResult::Write {
            completion: Some(EmbeddedTxnCompletion::WriteCommitted { commit_seq: CommitSeq(1) }), ..
        }));
        let mut found: Vec<_> = db.vertices_at(CommitSeq(1)).unwrap().into_iter()
            .map(|row| row.props).collect();
        found.sort();
        assert_eq!(found, vec![
            vec![(ID, CanonicalScalar::Int(1)), (NAME, CanonicalScalar::ucs_basic_text("updated\n'quote'; $aa").unwrap())],
            vec![(ID, CanonicalScalar::Int(2)), (NAME, CanonicalScalar::ucs_basic_text("second").unwrap())],
        ]);
        let update = "UNWIND $rows AS row MATCH (n:Entity {id:row.identity.id}) SET n.name=row.versions[0].name";
        db.query_write(&txcx, &cx, &commit, update, &args(vec![document(2, "changed")]),
            symbols, R, policy(), |_| -> Result<ElementId, Infallible> { panic!("SET allocates no identity") })
            .await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        let expected = db.query(&cx, "MATCH (n:Entity) RETURN n.id AS id,n.name AS name ORDER BY id",
            &GqlParameters::new(), symbols, read_policy()).unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &dir, keys()).await.unwrap();
        assert_eq!(db.query(&cx, "MATCH (n:Entity) RETURN n.id AS id,n.name AS name ORDER BY id",
            &GqlParameters::new(), symbols, read_policy()).unwrap(), expected);
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_malformed_late_document_cannot_execute_or_allocate_a_valid_prefix() {
    let ((), report) = run_async_under_lab(0x6e65_7302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let arguments = args(vec![document(1, "valid"), object(vec![
            ("identity", object(vec![("id", int(2))])), ("versions", int(3)),
        ])]);
        let result = db.query_write(&contexts.txn(), &cx, &commit, QUERY, &arguments,
            |_, _| panic!("no catalog before whole-input validation"), R, policy(),
            |_| -> Result<ElementId, Infallible> { panic!("no allocation before whole-input validation") })
            .await;
        assert!(matches!(result, Err(QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
            row: 1, kind: GraphUnwindRowError::ExpectedListField, ..
        }))));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_batch_execution_refusal_preserves_the_outer_transaction_workspace() {
    let ((), report) = run_async_under_lab(0x6e65_7303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut next = 100_u128;
        txn.query_write(&mut db, &cx, QUERY, &args(vec![document(1, "one"), document(2, "two")]),
            symbols, R, policy(), |_| { next += 1; Ok::<_, Infallible>(ElementId::Vertex(VId(next))) })
            .unwrap();
        assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
        let query = "MATCH (n:Entity) RETURN n.id AS id,n.name AS name,n.score AS score ORDER BY id";
        let before = txn.query(&db, &cx, query, &GqlParameters::new(), symbols, read_policy()).unwrap();
        let arguments = args(vec![
            object(vec![("identity", object(vec![("id", int(1))])), ("divisors", list(vec![int(2)]))]),
            object(vec![("identity", object(vec![("id", int(2))])), ("divisors", list(vec![int(0)]))]),
        ]);
        let result = txn.query_write(&mut db, &cx,
            "UNWIND $rows AS row MATCH (n:Entity {id:row.identity.id}) SET n.score=100/row.divisors[0]",
            &arguments, symbols, R, policy(),
            |_| -> Result<ElementId, Infallible> { panic!("SET allocates no identity") });
        assert!(matches!(result, Err(QueryWriteError::Execute(_))), "late arithmetic must fail in the executor");
        assert_eq!(txn.query(&db, &cx, query, &GqlParameters::new(), symbols, read_policy()).unwrap(), before);
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: CommitSeq(1) }));
        assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 2);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
