//! Native ingestion through a capability-only session, not a raw Database writer.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, IdentityPermutation, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::unwind_write::{
    GraphUnwindBindEvent, GraphUnwindRowError, GraphUnwindWriteError, GraphUnwindWriteText,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    GraphWriteScriptBatchError, GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Rights, Scope};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

type Fault = GraphWriteScriptExecutionError<WriteTxnError, WriteTxnError, WriteTxnError>;
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc2; 32]);
const NOW: u64 = 100;
const BRANCH: &str = "host-branch";
const COUNTER: &str = "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) \
    ON CREATE SET n.q=0 SET n.q=n.q+row.q";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], NS, [0xc3; 32])
}
/// The vertex identity the engine issues for `counter` under [`keys`].
fn engine_vertex(counter: u64) -> VId {
    VId(u128::from(
        IdentityPermutation::vertices(&keys())
            .permute(counter)
            .unwrap(),
    ))
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(10121), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: BRANCH.into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 10_000_000,
            max_rows: 1_000,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 1_000_000),
        1_000,
        1_000,
        1_000,
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
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
fn authorization(error: &Fault) -> Option<Error> {
    let mut source: Option<&(dyn core::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        source = error.source();
    }
    None
}
fn lab<F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_session_ingestion_and_existing_prepared_writes_survive_reopen() {
    lab(0xac21, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let calls = AtomicUsize::new(0);
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                |kind, name| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    symbols(kind, name)
                },
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let arguments = rows(&[(1, 2), (2, 4), (1, 3)]);
        let frozen = arguments.canonical_bytes();
        let (receipt, completion) = session.query(&query, COUNTER, &arguments).await.unwrap();
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().mutation_effects, 5);
        let first = receipt.steps()[0].merged_vertex().unwrap().vertex();
        let second = receipt.steps()[1].merged_vertex().unwrap().vertex();
        assert_eq!(receipt.steps()[2].merged_vertex().unwrap().vertex(), first);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(basis.0 + 1),
            }
        );
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert!(pinned.vertex(first).unwrap().is_none());
        let args = GqlParameters::new()
            .with_int64("key", 1)
            .unwrap()
            .with_int64("step", 10)
            .unwrap();
        let prepared = session
            .prepare(
                &query,
                "MATCH (n:Visible {p:$key}) SET n.q=n.q+$step",
                &args,
            )
            .unwrap();
        let resolved = calls.load(Ordering::Relaxed);
        session.execute(&query, &prepared, &args).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), resolved);
        assert!(!session.is_closed());
        drop(session);
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 2));
        assert_eq!(
            db.vertex(first).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(15))]
        );
        assert_eq!(
            db.vertex(second).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(4))]
        );
        let expected = db.vertices().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap(), expected);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn stats_only_ingestion_exceeds_the_default_64_only_when_the_host_admits_it() {
    lab(0xac22, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let mut scope = grant();
        scope.limits.max_rows = 0;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                65,
                || NOW,
            )
            .unwrap();
        let (stats, completion) = session
            .query_stats(&query, COUNTER, &rows(&[(1, 1); 65]))
            .await
            .unwrap();
        assert_eq!(stats.completed_statements, 65);
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(stats.mutation_effects, 66);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(basis.0 + 1),
            }
        );
        assert!(!session.is_closed());
        // Identical write execution with an identity receipt must still refuse
        // the zero signed row allowance and roll back this command's increment.
        let error = session
            .query(&query, COUNTER, &rows(&[(1, 1)]))
            .await
            .unwrap_err();
        assert_eq!(
            authorization(&error),
            Some(Error::LimitExceeded(LimitDimension::Rows))
        );
        assert!(session.is_closed());
        drop(session);
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        let vertices = db.vertices().unwrap();
        assert_eq!(vertices.len(), 1);
        assert_eq!(
            vertices[0].props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(65))]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn expanded_host_limits_precede_catalog_and_allocation_and_cannot_be_retried() {
    lab(0xac23, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for (limit, inputs) in [
            (0, vec![(1, 1)]),
            (1, vec![(1, 1), (2, 1)]),
            (64, vec![(1, 1); 65]),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let calls = AtomicUsize::new(0);
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    BRANCH,
                    |kind, name| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        symbols(kind, name)
                    },
                    R,
                    policy(),
                    limit,
                    || NOW,
                )
                .unwrap();
            let error = session
                .query(&query, COUNTER, &rows(&inputs))
                .await
                .unwrap_err();
            assert!(
                matches!(error, Fault::UnwindBinding(GraphUnwindWriteError::TooManyRows {
                limit: admitted, observed,
            }) if admitted == limit && observed == inputs.len())
            );
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(session.is_closed());
            let stopped = session
                .query_stats(&query, "CREATE (n)", &GqlParameters::new())
                .await
                .unwrap_err();
            assert_eq!(authorization(&stopped), Some(Error::ExecutionStopped));
            drop(session);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            // Nothing refused above took a counter: this fresh handle's first
            // engine allocation is still vertex counter 1.
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(engine_vertex(1))
            );
        }
        // Ordinary text must not lose the host cap while adopting native binding.
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                1,
                || NOW,
            )
            .unwrap();
        let error = session
            .query_stats(&query, "CREATE (n); CREATE (m)", &GqlParameters::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::BatchBinding(GraphWriteScriptBatchError::TooManyStatements {
                limit: 1,
                observed: 2,
            })
        ));
        assert!(session.is_closed());
        drop(session);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn rights_and_row_type_refusals_close_before_catalog_or_graph_work() {
    lab(0xac24, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        for write_only in [true, false] {
            let mut scope = grant();
            if write_only {
                scope.rights = Rights::Write;
            }
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let calls = AtomicUsize::new(0);
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    BRANCH,
                    |kind, name| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        symbols(kind, name)
                    },
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            let params = if write_only {
                GqlParameters::new()
            } else {
                GqlParameters::new()
                    .with_list(
                        "rows",
                        vec![
                            row(1, CanonicalScalar::Int(1)),
                            row(2, CanonicalScalar::Bool(true)),
                        ],
                    )
                    .unwrap()
            };
            let error = session.query(&query, COUNTER, &params).await.unwrap_err();
            if write_only {
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            } else {
                assert!(matches!(
                    error,
                    Fault::UnwindBinding(GraphUnwindWriteError::Row {
                        row: 1,
                        kind: GraphUnwindRowError::IncompatibleFieldTypes,
                        ..
                    })
                ));
            }
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(session.is_closed());
            drop(session);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn late_batch_failure_keeps_row_coordinates_and_only_prior_commits() {
    lab(0xac25, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(90),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(90)),
                (Q, CanonicalScalar::Int(i64::MAX)),
            ],
        );
        db.write(&commit, seed).await.unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let error = session
            .query(&query, COUNTER, &rows(&[(1, 2), (90, 1)]))
            .await
            .unwrap_err();
        let Fault::BatchProgram {
            location: Some(location),
            ..
        } = error
        else {
            panic!("expected the original input-row location")
        };
        assert_eq!(location.argument_set, 1);
        assert_eq!(location.statement, 0);
        assert_eq!(location.span, 0..COUNTER.len());
        assert!(session.is_closed());
        drop(session);
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn catalog_expiry_is_not_a_syntax_error_and_cannot_be_retried() {
    lab(0xac26, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let time = AtomicU64::new(NOW);
        let calls = AtomicUsize::new(0);
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                |kind, name| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    time.store(10_000, Ordering::Relaxed);
                    symbols(kind, name)
                },
                R,
                policy(),
                64,
                || time.load(Ordering::Relaxed),
            )
            .unwrap();
        let error = session
            .query(&query, COUNTER, &rows(&[(1, 2)]))
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::Expired));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(session.is_closed());
        time.store(NOW, Ordering::Relaxed);
        let error = session
            .query(&query, COUNTER, &rows(&[(1, 2)]))
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ExecutionStopped));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(session);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn frozen_unwind_inputs_and_receipts_reuse_without_catalog_or_rebinding() {
    lab(0xac27, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let catalog_open = AtomicBool::new(true);
        let calls = AtomicUsize::new(0);
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                |kind, name| {
                    assert!(
                        catalog_open.load(Ordering::Relaxed),
                        "bound execution reentered catalog"
                    );
                    calls.fetch_add(1, Ordering::Relaxed);
                    symbols(kind, name)
                },
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let mut arguments = rows(&[(1, 2), (2, 4), (1, 3)]);
        let bound = session
            .bind_unwind_batch(&query, COUNTER, &arguments)
            .unwrap();
        assert_eq!(bound.argument_sets(), 3);
        assert_eq!(bound.location(2).unwrap().argument_set, 2);
        assert_eq!(bound.location(2).unwrap().span, 0..COUNTER.len());
        assert!(bound.location(3).is_none());
        assert_eq!(
            txn.outstanding_obligations(),
            0,
            "binding cannot pin a transaction"
        );
        assert!(!format!("{bound:?}").contains("Visible"));
        let frozen_calls = calls.load(Ordering::Relaxed);
        assert!(frozen_calls > 0);
        // Replace and drop the caller's map; the handle owns its original values.
        arguments = rows(&[(99, 999)]);
        assert_eq!(arguments.len(), 1);
        drop(arguments);
        catalog_open.store(false, Ordering::Relaxed);
        let (receipt, completion) = session.execute_bound_batch(&query, &bound).await.unwrap();
        let first = receipt.steps()[0].merged_vertex().unwrap().vertex();
        let second = receipt.steps()[1].merged_vertex().unwrap().vertex();
        assert_eq!(receipt.steps()[2].merged_vertex().unwrap().vertex(), first);
        let record = bound.record_receipts(&receipt, 2).unwrap();
        assert_eq!(record.len(), 1);
        assert_eq!(record[0].merged_vertex().unwrap().vertex(), first);
        assert!(bound.record_receipts(&receipt, 3).is_none());
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(basis.0 + 1),
            }
        );
        // Reusing the same handle is another complete write, never deduplication.
        let (stats, completion) = session
            .execute_bound_batch_stats(&query, &bound)
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(stats.completed_statements, 3);
        assert_eq!(stats.mutation_effects, 3);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(basis.0 + 2),
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), frozen_calls);
        assert!(!session.is_closed());
        drop(session);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(
            db.vertex(first).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(10))]
        );
        assert_eq!(
            db.vertex(second).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(8))]
        );
        let expected = db.vertices().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap(), expected);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn frozen_batch_preparation_neither_allocates_graph_ids_nor_accepts_nonbatch_fallbacks() {
    lab(0xac28, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    R,
                    policy(),
                    if mode == 1 { 1 } else { 64 },
                    || NOW,
                )
                .unwrap();
            let result = match mode {
                2 => session.bind_unwind_batch(
                    &query,
                    "CREATE (n:Visible {p:1})",
                    &GqlParameters::new(),
                ),
                3 => session.bind_unwind_batch(
                    &query,
                    COUNTER,
                    &GqlParameters::new()
                        .with_list(
                            "rows",
                            vec![
                                row(1, CanonicalScalar::Int(1)),
                                row(2, CanonicalScalar::Bool(true)),
                            ],
                        )
                        .unwrap(),
                ),
                _ => session.bind_unwind_batch(&query, COUNTER, &rows(&[(1, 2), (2, 3)])),
            };
            if mode == 0 {
                assert_eq!(result.unwrap().argument_sets(), 2);
                assert!(!session.is_closed());
            } else {
                let error = result.unwrap_err();
                match mode {
                    1 => assert!(matches!(
                        error,
                        Fault::UnwindBinding(GraphUnwindWriteError::TooManyRows {
                            limit: 1,
                            observed: 2,
                        })
                    )),
                    2 => assert!(matches!(
                        error,
                        Fault::Program(fgdb_gql::GraphWriteProgramError::Program(
                            fgdb_gql::GraphMutationProgramError::Preflight(
                                WriteTxnError::AuthorizedMutationRefused
                            )
                        ))
                    )),
                    _ => assert!(matches!(
                        error,
                        Fault::UnwindBinding(GraphUnwindWriteError::Row { row: 1, .. })
                    )),
                }
                assert!(session.is_closed());
            }
            drop(session);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            // Preparation took no counter: this fresh handle's first engine
            // allocation is still vertex counter 1.
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(engine_vertex(1))
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn frozen_unwind_handle_cannot_cross_identical_sessions() {
    lab(0xac29, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut first = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let bound = first
            .bind_unwind_batch(&query, COUNTER, &rows(&[(1, 2)]))
            .unwrap();
        drop(first);
        let calls = AtomicUsize::new(0);
        let mut second = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                |kind, name| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    symbols(kind, name)
                },
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let error = second
            .execute_bound_batch(&query, &bound)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::Program(fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(
                    WriteTxnError::AuthorizedMutationRefused
                )
            ))
        ));
        assert!(second.is_closed());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        drop(second);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        // Neither the binding nor the refused execution took a counter.
        assert_eq!(
            db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                .unwrap(),
            ElementId::Vertex(engine_vertex(1))
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn frozen_input_never_freezes_credentials_or_the_clock_high_water_mark() {
    lab(0xac2a, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        for mode in 0..3 {
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let time = AtomicU64::new(NOW);
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    R,
                    policy(),
                    64,
                    || time.load(Ordering::Relaxed),
                )
                .unwrap();
            time.store(200, Ordering::Relaxed);
            let bound = session
                .bind_unwind_batch(&query, COUNTER, &rows(&[(1, 2)]))
                .unwrap();
            let expected = match mode {
                0 => {
                    time.store(150, Ordering::Relaxed);
                    Error::ClockWentBackwards
                }
                1 => {
                    time.store(10_000, Ordering::Relaxed);
                    Error::Expired
                }
                _ => {
                    assert!(issuer.retire());
                    Error::AuthorityRetired
                }
            };
            let error = session
                .execute_bound_batch_stats(&query, &bound)
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(expected));
            assert!(session.is_closed());
            drop(session);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn frozen_batch_runtime_failure_has_original_coordinates_and_no_durable_prefix() {
    lab(0xac2b, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(90),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(90)),
                (Q, CanonicalScalar::Int(i64::MAX)),
            ],
        );
        db.write(&commit, seed).await.unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        // Binding does not evaluate the existing graph's overflowing counter.
        let bound = session
            .bind_unwind_batch(&query, COUNTER, &rows(&[(1, 2), (90, 1)]))
            .unwrap();
        let error = session
            .execute_bound_batch(&query, &bound)
            .await
            .unwrap_err();
        let Fault::BatchProgram {
            location: Some(location),
            ..
        } = error
        else {
            panic!("bound ingestion must preserve its original failing record")
        };
        assert_eq!(location, bound.location(1).unwrap());
        assert_eq!(location.span, 0..COUNTER.len());
        assert!(session.is_closed());
        drop(session);
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn unwinding_while_preparing_a_frozen_batch_closes_without_escaping_a_prefix() {
    lab(0xac2c, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let calls = AtomicUsize::new(0);
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                |_, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    panic!("injected preparation callback unwind")
                },
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let arguments = rows(&[(1, 2), (2, 3)]);
        // Only the unwind matters here; `.is_ok()` keeps the closure's result
        // small (clippy::result_large_err rejects the 160-byte error type).
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session
                .bind_unwind_batch(&query, COUNTER, &arguments)
                .is_ok()
        }));
        assert!(failure.is_err());
        assert!(session.is_closed());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let error = session
            .bind_unwind_batch(&query, COUNTER, &arguments)
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ExecutionStopped));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(session);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn explicit_preparation_is_separate_but_query_must_not_refresh_its_binding_allowance() {
    lab(0xac2d, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let text = "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) SET n.q=row.q";
        let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(4096)).unwrap();
        let arguments = GqlParameters::new()
            .with_list("rows", vec![row(1, payload.clone()), row(1, payload)])
            .unwrap();
        let mut binding_work = 0;
        GraphUnwindWriteText::parse(text)
            .unwrap()
            .bind_with_limit_controlled(&arguments, R, 64, symbols, |event| {
                if let GraphUnwindBindEvent::Work(units) = event {
                    binding_work += units;
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        let mut scope = grant();
        // Session entry, original text, post-classification, and the complete
        // controlled binder. Nothing remains for the native execution phase.
        scope.limits.max_work = text.len() as u64 + 2 + binding_work;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let error = session.query(&query, text, &arguments).await.unwrap_err();
        assert!(matches!(
            authorization(&error),
            Some(Error::LimitExceeded(_))
        ));
        assert!(matches!(error, Fault::BatchProgram { location: None, .. }));
        assert!(session.is_closed());
        drop(session);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        // One extra unit admits bind_unwind_batch's final handle acceptance.
        // Each execute_bound_batch is explicitly a NEW operation, not a hidden
        // reset inside query. This budget is sufficient for the write itself.
        scope.limits.max_work += 1;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                BRANCH,
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let bound = session.bind_unwind_batch(&query, text, &arguments).unwrap();
        let (receipt, _) = session.execute_bound_batch(&query, &bound).await.unwrap();
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert!(!session.is_closed());
        drop(session);
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
