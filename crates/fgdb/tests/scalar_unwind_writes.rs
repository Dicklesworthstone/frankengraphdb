//! Scalar and matrix batches enter the real native write facade. Expected
//! stored effects are specified independently of the binder's generated text.
//! Every successful batch has one native completion, never one per item.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteIdentityRequest,
    GraphWriteProgramPolicy, GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const ENTITY: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const UPSERT: &str = "UNWIND $rows AS x MERGE (n:Entity {id:x}) \
    ON CREATE SET n.p=1 ON MATCH SET n.p=n.p+1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(ENTITY)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}
fn policy(effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000), effects, vertices, 0,
    )
}
fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn list(values: Vec<GraphValue>) -> GraphValue { GraphValue::List(values.into_boxed_slice()) }
fn object(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(name, value)| (name.into(), value)).collect()).unwrap()
}
fn args(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}
fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, Infallible> {
    panic!("this MATCH mutation cannot allocate a graph identity")
}
fn staged(result: QueryResult, statements: usize) {
    let QueryResult::Write { receipt, completion } = result else {
        panic!("no-RETURN write must keep its ordinary program receipt")
    };
    assert!(completion.is_none(), "staging is not durability");
    assert_eq!(receipt.stats().completed_statements, statements);
}

#[test]
fn scalar_upserts_matrix_updates_and_correlated_tags_publish_one_durable_commit() {
    let ((), report) = run_async_under_lab(0x7363_6101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let issued = AtomicU64::new(0);
        let input = args(vec![int(2), int(1), int(2)]);
        let frozen = input.canonical_bytes();
        staged(txn.query_write(&mut db, &cx, UPSERT, &input, symbols, R, policy(10, 2), |request| {
            assert!(request.statement < 2, "the third record reuses the first vertex");
            assert!(matches!(request.request, GraphInsertRequest::Vertex { row: 0, vertex: 0 }));
            let offset = issued.fetch_add(1, Ordering::Relaxed);
            Ok::<_, Infallible>(ElementId::Vertex(VId(u128::MAX - u128::from(offset))))
        }).unwrap(), 3);
        assert_eq!(issued.load(Ordering::Relaxed), 2);
        assert_eq!(input.canonical_bytes(), frozen);
        assert_eq!(txn.vertex(&db, VId(u128::MAX)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(2)), (P, CanonicalScalar::Int(2))]);
        assert_eq!(txn.vertex(&db, VId(u128::MAX - 1)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(1))]);

        let matrix = args(vec![list(vec![int(1), int(1)]), list(vec![]), list(vec![int(2)])])
            .with_int64("step", 1).unwrap();
        staged(txn.query_write(&mut db, &cx,
            "UNWIND $rows AS group UNWIND group AS x MATCH (n:Entity {id:x}) SET n.p=n.p+$step",
            &matrix, symbols, R, policy(3, 0), no_allocation).unwrap(), 3);
        let final_name = "second; 'quoted' $aa\nnext";
        let tags = args(vec![
            object(vec![("id", int(1)), ("tags", list(vec![text("first"), text(final_name)]))]),
            object(vec![("id", int(2)), ("tags", list(vec![null(), text("two")]))]),
        ]);
        staged(txn.query_write(&mut db, &cx,
            "UNWIND $rows AS row UNWIND row.tags AS tag MATCH (n:Entity {id:row.id}) SET n.name=tag",
            &tags, symbols, R, policy(4, 0), no_allocation).unwrap(), 4);
        assert!(db.vertices().unwrap().is_empty(), "no item publishes its own commit");
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        for (vertex, id, name) in [(u128::MAX, 2, "two"), (u128::MAX - 1, 1, final_name)] {
            assert_eq!(txn.vertex(&db, VId(vertex)).unwrap().unwrap().props,
                vec![(ID, CanonicalScalar::Int(id)), (P, CanonicalScalar::Int(3)),
                    (NAME, CanonicalScalar::ucs_basic_text(name).unwrap())]);
        }
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: CommitSeq(1) }));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.vertices().unwrap().len(), 2);
        for (vertex, id, name) in [(u128::MAX, 2, "two"), (u128::MAX - 1, 1, final_name)] {
            assert_eq!(db.vertex(VId(vertex)).unwrap().unwrap().props,
                vec![(ID, CanonicalScalar::Int(id)), (P, CanonicalScalar::Int(3)),
                    (NAME, CanonicalScalar::ucs_basic_text(name).unwrap())]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_late_scalar_execution_failure_restores_the_outer_prefix_and_original_record_location() {
    let ((), report) = run_async_under_lab(0x7363_6102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![ENTITY], vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))]);
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), P, CanonicalScalar::Int(77));
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let query = "UNWIND /* é */ $rows AS x MATCH (n:Entity {id:$target}) SET n.p=100/x;";
        let invalid = args(vec![int(2), int(0)]).with_int64("target", 1).unwrap();
        let error = txn.query_write(&mut db, &cx, query, &invalid, symbols, R, policy(2, 0),
            no_allocation).unwrap_err();
        let QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) = error else { panic!("wrong execution error: {error:?}") };
        assert_eq!(location.argument_set, 1);
        assert_eq!(location.statement, 0);
        assert_eq!(location.span, 0..query.find(';').unwrap());
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertex(&db, VId(1)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(77))]);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))]);
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);

        // The autocommit facade likewise cannot publish its successful first
        // item when the next item fails. Its private obligation is discharged.
        let error = db.query_write(&txcx, &cx, &commit, query, &invalid, symbols, R,
            policy(2, 0), no_allocation).await.unwrap_err();
        assert!(matches!(error, QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) if location.argument_set == 1 && location.span == (0..query.find(';').unwrap())));
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(77))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
        let valid = args(vec![int(4), int(5)]).with_int64("target", 1).unwrap();
        let result = db.query_write(&txcx, &cx, &commit, query, &valid, symbols, R,
            policy(2, 0), no_allocation).await.unwrap();
        assert!(matches!(result, QueryResult::Write { completion: Some(
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: CommitSeq(3) }), .. }));
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(20))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_scalar_matrix_and_overexpanded_sources_refuse_before_catalog_or_allocation() {
    let ((), report) = run_async_under_lab(0x7363_6103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let direct = "UNWIND $rows AS x MATCH (n:Entity) SET n.p=x";
        let matrix = "UNWIND $rows AS m UNWIND m AS x MATCH (n:Entity) SET n.p=x";
        let product = "UNWIND $rows AS m UNWIND m AS x UNWIND m AS y \
            MATCH (n:Entity {id:x}) SET n.p=y";
        let cases = [
            (direct, args(vec![int(1), GraphValue::Scalar(CanonicalScalar::Bool(true))])),
            (direct, args(vec![int(1), object(vec![("p", int(2))])])),
            (matrix, args(vec![list(vec![int(1)]), int(2)])),
            (product, args(vec![list((0..257).map(int).collect())])),
        ];
        for (at, (query, input)) in cases.iter().enumerate() {
            let frozen = input.canonical_bytes();
            let error = db.query_write(&contexts.txn(), &cx, &commit, query, input,
                |_, _| panic!("input admission must precede every catalog callback"), R,
                policy(100_000, 100_000), no_allocation).await.unwrap_err();
            match (at, error) {
                (0, QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
                    row: 1, kind: GraphUnwindRowError::IncompatibleFieldTypes, .. })) => {}
                (1, QueryWriteError::UnwindBinding(GraphUnwindWriteError::Row {
                    row: 1, kind: GraphUnwindRowError::ExpectedScalarField, .. })) => {}
                (2, QueryWriteError::UnwindBinding(GraphUnwindWriteError::Expansion {
                    row: 1, clause: 1, kind: GraphUnwindRowError::ExpectedListField, .. })) => {}
                (3, QueryWriteError::UnwindBinding(GraphUnwindWriteError::TooManyRows {
                    limit: 65_536, observed: 65_537 })) => {}
                (_, error) => panic!("wrong admission refusal: {error:?}"),
            }
            assert_eq!(input.canonical_bytes(), frozen);
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_batches_share_creation_and_effect_budgets_and_keep_issued_identities_consumed() {
    let ((), report) = run_async_under_lab(0x7363_6104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issued = AtomicU64::new(100);
        let allocate = |_: GraphWriteIdentityRequest| {
            Ok::<_, Infallible>(ElementId::Vertex(VId(u128::from(
                issued.fetch_add(1, Ordering::Relaxed),
            ))))
        };
        let values = args(vec![int(1), int(2)]);
        let error = db.query_write(&txcx, &cx, &commit, UPSERT, &values, symbols, R,
            policy(10, 1), allocate).await.unwrap_err();
        assert!(matches!(error, QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) if location.argument_set == 1));
        assert!(issued.load(Ordering::Relaxed) > 100, "first item already issued an identity");
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert_eq!(txcx.outstanding_obligations(), 0);
        let first_available = issued.load(Ordering::Relaxed);
        let result = db.query_write(&txcx, &cx, &commit, UPSERT, &values, symbols, R,
            policy(2, 2), allocate).await.unwrap();
        assert!(matches!(result, QueryResult::Write { completion: Some(
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: CommitSeq(1) }), .. }));
        let current = db.vertices().unwrap();
        assert_eq!(current.len(), 2);
        for id in 100..first_available {
            assert!(db.vertex(VId(u128::from(id))).unwrap().is_none(), "failed program identities stay unused");
        }
        let query = "UNWIND $rows AS x MATCH (n:Entity {id:x}) SET n.p=n.p+1";
        let error = db.query_write(&txcx, &cx, &commit, query, &values, symbols, R,
            policy(1, 0), no_allocation).await.unwrap_err();
        assert!(matches!(error, QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location), ..
        }) if location.argument_set == 1));
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        let mut properties = db.vertices().unwrap().into_iter().map(|row| row.props).collect::<Vec<_>>();
        properties.sort();
        assert_eq!(properties, vec![
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(1))],
            vec![(ID, CanonicalScalar::Int(2)), (P, CanonicalScalar::Int(1))],
        ]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
