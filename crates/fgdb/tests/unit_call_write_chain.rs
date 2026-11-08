//! Unit CALL subqueries retain outer occurrences and one atomic, scoped owner.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};
use std::convert::Infallible;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc1; 32]);
const R: RelationId = RelationId(1);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc0; 32], NS, [0xc2; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(VISIBLE)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn program(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn policy(rows: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, rows, 2_000_000, 1_000_000),
        100,
        100,
        100,
    )
}

fn insert_policy(rows: u64) -> GraphInsertPolicy {
    policy(rows).insertion_policy()
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xca11), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rights: Rights, rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 2_000_000,
            max_rows: rows,
        },
        expires_at_ms: 10_000,
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn values(result: QueryResult, expected: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("a terminal RETURN must use the public row result");
    };
    assert_eq!(columns, expected);
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value,
                    other => panic!("expected native value, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

#[test]
fn public_unit_call_preserves_duplicate_outer_rows_and_commits_all_body_rows_once() {
    let ((), report) = run_async_under_lab(0xca11_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let pinned = db.read_session().unwrap();
        let basis = db.frontier().unwrap();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "UNWIND [1,1,2] AS x CALL { WITH x UNWIND [3,4] AS y \
                 CREATE (:Visible {p:x*10+y}) } RETURN x ORDER BY x",
                &GqlParameters::new(),
                symbols,
                R,
                policy(3),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["x"]),
            vec![vec![int(1)], vec![int(1)], vec![int(2)]]
        );
        assert_eq!(next, 106);
        let expected = db.vertices().unwrap();
        assert_eq!(expected.len(), 6);
        let mut properties: Vec<_> = expected.iter().map(|row| row.props.clone()).collect();
        properties.sort();
        assert_eq!(
            properties,
            [13, 13, 14, 14, 23, 24]
                .into_iter()
                .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                .collect::<Vec<_>>()
        );
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert!(pinned.vertices().unwrap().is_empty());
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_call_restores_outer_scope_and_refreshes_mutated_elements() {
    let ((), report) = run_async_under_lab(0xca11_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "CREATE (n:Visible {p:0}) WITH n,n.p AS saved \
                 UNWIND [1,1,2] AS x CALL { WITH n,x \
                 CALL { WITH n,x SET n.p=n.p+x } } \
                 RETURN n.p AS current,saved,x ORDER BY x",
                &GqlParameters::new(),
                symbols,
                R,
                policy(3),
                |_| Ok::<_, Infallible>(ElementId::Vertex(VId(100))),
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["current", "saved", "x"]),
            [1, 1, 2]
                .into_iter()
                .map(|x| vec![int(4), int(0), int(x)])
                .collect::<Vec<_>>()
        );
        assert_eq!(
            db.vertex(VId(100)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(4).into())]
        );
        // Without imports the inner variable is local, even if its name is
        // also used by the outer scalar. Only the original scalar escapes.
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "WITH 99 AS x CALL { CREATE (x:Visible {p:1}) \
                 CALL { WITH x SET x.q=x.p+1 } } RETURN x",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1),
                |_| Ok::<_, Infallible>(ElementId::Vertex(VId(101))),
            )
            .await
            .unwrap();
        assert_eq!(values(result, &["x"]), vec![vec![int(99)]]);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_inner_match_retains_occurrences_and_reads_only_authorized_vertices() {
    let ((), report) = run_async_under_lab(0xca11_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, label) in [(1, VISIBLE), (2, HIDDEN)] {
            seed.create_vertex(
                VId(id),
                vec![label],
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(0))],
            );
        }
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let (_, rows, _) = db
            .execute_graph_write_chain_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &issuer,
                &token,
                "main",
                &program(
                    "UNWIND [1,1,2] AS x CALL { WITH x \
                    MATCH (n {p:x}) SET n.q=n.q+1 } RETURN x ORDER BY x",
                ),
                insert_policy(3),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            rows.value
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            vec![vec![int(1)], vec![int(1)], vec![int(2)]]
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(2).into())
            ]
        );
        assert_eq!(
            db.vertex(VId(2)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(0).into())
            ]
        );
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_body_failure_restores_the_existing_transaction_prefix() {
    let ((), report) = run_async_under_lab(0xca11_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![VISIBLE], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let before = transaction.staged_effect_digest().unwrap();
        let mut allocated = 0;
        transaction
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &program(
                    "UNWIND [1,2] AS x CALL { WITH x CREATE (:Visible {p:10/(2-x)}) } RETURN x",
                ),
                insert_policy(2),
                |request| {
                    allocated += 1;
                    Ok::<_, Infallible>(match request {
                        GraphInsertRequest::Vertex { .. } => {
                            ElementId::Vertex(VId(100 + allocated))
                        }
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(100 + allocated)),
                    })
                },
            )
            .expect_err("a later correlated failure must discard all earlier body writes");
        assert_eq!(allocated, 1, "the first outer occurrence completed");
        assert_eq!(transaction.staged_effect_digest().unwrap(), before);
        assert_eq!(transaction.vertices(&db).unwrap().len(), 1);
        assert_eq!(
            transaction.vertex(&db, VId(7)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(99).into())]
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        transaction
            .finish(&mut db, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_calls_cannot_hide_denied_reads_and_nested_calls_share_all_limits() {
    let ((), report) = run_async_under_lab(0xca11_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let issuer = authority();
        for (text, rights, denial) in [
            (
                "UNWIND [] AS x CALL { WITH x MATCH (n:Visible) SET n.p=n.secret }",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "UNWIND [] AS x CALL { WITH x MATCH (n:Visible) SET n.p=1 }",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer.issue_at(&grant(rights, 0), NOW).unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &program(text),
                    insert_policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(denial), "{text}: {error:?}");
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }
        let call = program(
            "UNWIND [1,2,3] AS x CALL { WITH x \
            CALL { WITH x CREATE (:Visible {p:x}) } } RETURN x",
        );
        for (limits, rows) in [
            (GraphInsertPolicy::new(insert_policy(3).query, 2, 0), 3),
            (insert_policy(2), 3),
            (insert_policy(3), 2),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, rows), NOW)
                .unwrap();
            db.execute_graph_write_chain_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &issuer,
                &token,
                "main",
                &call,
                limits,
                || NOW,
            )
            .await
            .expect_err("a nested CALL must not mint a fresh creation or result allowance");
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_and_late_authority_expiry_discard_every_call_frame() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let ((), report) = run_async_under_lab(0xca11_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let call = program(
            "UNWIND [1,2,3] AS x CALL { WITH x \
            CREATE (n:Visible {p:x}) CALL { WITH n SET n.q=n.p+1 } } RETURN x",
        );
        let observed = Arc::new(SimulationCheckpointProbe::new(None));
        let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
        baseline
            .execute_graph_write_chain_authorized(
                &txn,
                &query.with_checkpoint_probe(Arc::clone(&observed)),
                &commit,
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(3),
                || NOW,
            )
            .await
            .unwrap();
        let calls = observed.calls();
        assert!(calls > 3);
        for stop in [calls / 2, calls - 1] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            db.execute_graph_write_chain_authorized(
                &txn,
                &query.with_checkpoint_probe(Arc::clone(&probe)),
                &commit,
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(3),
                || NOW,
            )
            .await
            .expect_err("cancellation aborts every scoped body");
            assert_eq!(probe.calls(), stop);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }
        let expired = Arc::new(AtomicBool::new(false));
        let clock = Arc::clone(&expired);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(3),
                move || {
                    if clock.load(Ordering::Relaxed) {
                        10_000
                    } else {
                        NOW
                    }
                },
                |rows, _| {
                    assert_eq!(rows.len(), 3);
                    expired.store(true, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(expired.load(Ordering::Relaxed));
        assert_eq!(authorization(&error), Some(Error::Expired));
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
