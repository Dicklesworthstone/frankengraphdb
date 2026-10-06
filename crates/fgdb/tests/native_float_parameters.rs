//! Float parameters traverse native preparation, real storage and authorized writes.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue, QueryWriteError,
    WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error as AuthorizationError, Grant, QueryLimits, Rights, Scope};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const NODE: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xd4; 32]);
const COUNTER: &str = "UNWIND $rows AS row MERGE (n:Node {id:row.id}) \
    ON CREATE SET n.p=0.0 SET n.p=n.p+row.rate";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd3; 32], NS, [0xd5; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(NODE)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}
fn parameter(value: CanonicalScalar) -> GqlParameters {
    GqlParameters::new().with_scalar("rate", value).unwrap()
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 100, 100, 100)
}
fn rows(values: &[(i64, f64)]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            values
                .iter()
                .map(|&(id, rate)| {
                    GraphValue::map(vec![
                        ("id".into(), GraphValue::Scalar(CanonicalScalar::Int(id))),
                        ("rate".into(), GraphValue::Scalar(float(rate))),
                    ])
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
}
fn cells(result: QueryResult) -> Vec<Vec<CanonicalScalar>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read result")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(GraphValue::Scalar(value)) => value,
                    _ => panic!("ordinary scalar result"),
                })
                .collect()
        })
        .collect()
}
fn property(db: &Database<MemVfs>, vertex: VId, key: PropertyKeyId) -> CanonicalScalar {
    db.vertex(vertex)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(found, _)| *found == key)
        .unwrap()
        .1
}
fn authorization(mut error: &(dyn core::error::Error + 'static)) -> Option<AuthorizationError> {
    loop {
        if let Some(fgdb::WriteTxnError::Authorization(cause)) = error.downcast_ref() {
            return Some(*cause);
        }
        error = error.source()?;
    }
}

#[test]
fn prepared_native_reads_rebind_float_parameters_without_resolving_again() {
    let ((), report) = run_async_under_lab(0xf10a_0101, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&cx.commit(), keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![NODE], vec![(P, CanonicalScalar::Int(2))]);
        seed.create_vertex(VId(2), vec![NODE], vec![(P, float(3.0))]);
        db.write(&cx.commit(), seed).await.unwrap();
        let calls = AtomicUsize::new(0);
        let text = "MATCH (n:Node) WHERE n.p+$rate > 2.25 RETURN n.p*$rate AS value ORDER BY value";
        let prepared = PreparedNativeRead::prepare(
            text,
            &parameter(float(0.5)),
            |kind: GraphSymbolKind, name: &str| {
                calls.fetch_add(1, Ordering::Relaxed);
                symbols(kind, name)
            },
        )
        .unwrap();
        let frozen_calls = calls.load(Ordering::Relaxed);
        for (rate, expected) in [(0.5, [1.0, 1.5]), (2.0, [4.0, 6.0])] {
            let arguments = parameter(float(rate));
            let frozen = arguments.canonical_bytes();
            let result = prepared
                .execute(&db, &cx.query(), &arguments, query_policy())
                .unwrap();
            assert_eq!(
                cells(result),
                expected.map(|value| vec![float(value)]).to_vec()
            );
            assert_eq!(arguments.canonical_bytes(), frozen);
            assert_eq!(calls.load(Ordering::Relaxed), frozen_calls);
        }
        let wrong = parameter(CanonicalScalar::ucs_basic_text("not a rate").unwrap());
        assert!(
            prepared
                .execute(&db, &cx.query(), &wrong, query_policy())
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), frozen_calls);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn float_parameter_upserts_persist_the_computed_value_after_compact_and_reopen() {
    let ((), report) = run_async_under_lab(0xf10a_0102, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let text = "MERGE (n:Node {id:1}) ON CREATE SET n.p=0.0 SET n.p=n.p+$rate";
        let mut chosen = None;
        for (rate, expected) in [(0.5, 0.5), (1.25, 1.75)] {
            let result = db
                .query_write_engine(
                    &cx.txn(),
                    &cx.query(),
                    &cx.commit(),
                    text,
                    &parameter(float(rate)),
                    symbols,
                    R,
                    write_policy(),
                )
                .await
                .unwrap();
            let QueryResult::Write {
                receipt,
                completion: Some(completion),
            } = result
            else {
                panic!("one completed native write")
            };
            let vertex = receipt.steps()[0].merged_vertex().unwrap().vertex();
            if let Some(previous) = chosen {
                assert_eq!(vertex, previous);
            }
            chosen = Some(vertex);
            assert_eq!(property(&db, vertex, P), float(expected));
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
        }
        let vertex = chosen.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 2);
        db.compact(&cx.commit()).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&cx.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(property(&db, vertex, P), float(1.75));
        assert_eq!(cx.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn inferred_float_row_fields_and_relationship_parameters_use_the_same_writer() {
    let ((), report) = run_async_under_lab(0xf10a_0103, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&cx.commit(), keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let arguments = rows(&[(1, 0.5), (2, 2.0), (1, 1.25)]);
        let result = db
            .query_write_engine(
                &cx.txn(),
                &cx.query(),
                &cx.commit(),
                COUNTER,
                &arguments,
                symbols,
                R,
                write_policy(),
            )
            .await
            .unwrap();
        let QueryResult::Write { receipt, .. } = result else {
            panic!("batch receipt")
        };
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        let first = receipt.steps()[0].merged_vertex().unwrap().vertex();
        assert_eq!(receipt.steps()[2].merged_vertex().unwrap().vertex(), first);
        assert_eq!(property(&db, first, P), float(1.75));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        let text = "MATCH (a:Node {id:1}),(b:Node {id:2}) MERGE (a)-[e:R]->(b) \
            ON CREATE SET e.p=0.0 SET e.p=e.p+$rate";
        for rate in [0.25, 0.5] {
            db.query_write_engine(
                &cx.txn(),
                &cx.query(),
                &cx.commit(),
                text,
                &parameter(float(rate)),
                symbols,
                R,
                write_policy(),
            )
            .await
            .unwrap();
        }
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].props, vec![(P, float(0.75))]);
        assert_eq!(cx.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_float_division_failure_keeps_its_input_location_and_outer_prefix() {
    let ((), report) = run_async_under_lab(0xf10a_0104, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&cx.commit(), keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&cx.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(
            VId(90),
            vec![NODE],
            vec![(ID, CanonicalScalar::Int(90)), (P, float(9.0))],
        );
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let text = "UNWIND $rows AS row MERGE (n:Node {id:row.id}) SET n.p=$numerator/row.rate";
        let arguments = rows(&[(1, 2.0), (2, 0.0)])
            .with_scalar("numerator", float(6.0))
            .unwrap();
        let error = txn
            .query_write_engine(
                &mut db,
                &cx.query(),
                text,
                &arguments,
                symbols,
                R,
                write_policy(),
            )
            .unwrap_err();
        let QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location),
            source,
        }) = error
        else {
            panic!("failure must come from the executed second input record")
        };
        assert_eq!(location.argument_set, 1);
        assert_eq!(location.statement, 0);
        assert_eq!(location.span, 0..text.len());
        assert!(matches!(
            source,
            GraphWriteProgramError::VertexUpsert { .. }
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap().len(), 1);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &cx.commit()).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(property(&db, VId(90), P), float(9.0));
        assert_eq!(cx.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_float_batches_keep_hidden_fields_masked_and_denied_writes_atomic() {
    let ((), report) = run_async_under_lab(0xf10a_0105, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&cx.commit(), keys()).await.unwrap();
        let secret = CanonicalScalar::ucs_basic_text("not a numeric value").unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![NODE],
            vec![
                (ID, CanonicalScalar::Int(1)),
                (P, float(10.0)),
                (SECRET, secret.clone()),
            ],
        );
        db.write(&cx.commit(), seed).await.unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(0xf10a), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let token = authority
            .issue_at(
                &Grant {
                    branch: "main".into(),
                    labels: Scope::only([NODE]),
                    relations: Scope::only([R]),
                    properties: Scope::only([ID, P]),
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 100_000,
                        max_work: 10_000_000,
                        max_rows: 100,
                    },
                    expires_at_ms: 10_000,
                },
                100,
            )
            .unwrap();
        let text = "UNWIND $rows AS row MERGE (n:Node {id:row.id}) \
            SET n.p=n.p+COALESCE(n.secret,0.0)+row.rate";
        db.query_write_authorized(
            &cx.txn(),
            &cx.query(),
            &cx.commit(),
            &authority,
            &token,
            "main",
            text,
            &rows(&[(1, 0.5), (1, 1.25)]),
            symbols,
            R,
            write_policy(),
            || 100,
        )
        .await
        .unwrap();
        assert_eq!(property(&db, VId(1), P), float(11.75));
        assert_eq!(property(&db, VId(1), SECRET), secret);
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let forbidden = "UNWIND $rows AS row MERGE (n:Node {id:row.id}) \
            ON MATCH SET n.p=n.p+row.rate SET n.secret=row.rate*2.0";
        let error = db
            .query_write_authorized(
                &cx.txn(),
                &cx.query(),
                &cx.commit(),
                &authority,
                &token,
                "main",
                forbidden,
                &rows(&[(1, 0.5)]),
                symbols,
                R,
                write_policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(AuthorizationError::ScopeDenied));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(cx.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn integer_only_native_division_and_nullable_float_results_remain_distinct() {
    let ((), report) = run_async_under_lab(0xf10a_0106, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&cx.commit(), keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![NODE], vec![(P, CanonicalScalar::Int(5))]);
        db.write(&cx.commit(), seed).await.unwrap();
        for (arguments, expected) in [
            (
                GqlParameters::new().with_int64("rate", 2).unwrap(),
                CanonicalScalar::Int(2),
            ),
            (parameter(float(2.0)), float(2.5)),
            (parameter(CanonicalScalar::Null), CanonicalScalar::Null),
        ] {
            let result = db
                .query(
                    &cx.query(),
                    "MATCH (n:Node) RETURN n.p/$rate AS value",
                    &arguments,
                    symbols,
                    query_policy(),
                )
                .unwrap();
            assert_eq!(cells(result), vec![vec![expected]]);
        }
        let before = db.frontier().unwrap();
        assert!(
            db.query(
                &cx.query(),
                "MATCH (n:Node) RETURN n.p/$rate AS value",
                &parameter(float(0.0)),
                symbols,
                query_policy(),
            )
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
