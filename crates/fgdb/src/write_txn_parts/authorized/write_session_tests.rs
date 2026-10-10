//! Session ownership/lifetime tests through real Warden and Chronicle paths.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{GqlQueryPolicy, GraphMutationProgramError, GraphWriteProgramError};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Rights, Scope};
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xa1; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa0; 32], NS, [0xa2; 32])
}
fn authority() -> Authority {
    Authority::new(
        AuthKey::from_seed(10101),
        NS,
        "host-graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "host-branch".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 100_000),
        100,
        100,
        100,
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn arguments(key: i64, value: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("key", key)
        .unwrap()
        .with_int64("value", value)
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
fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}

#[test]
fn native_and_session_prepared_writes_use_one_commit_and_a_frozen_resolver() {
    lab(0xac01, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let calls = AtomicUsize::new(0);
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "host-branch",
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
        assert_eq!(
            txn.outstanding_obligations(),
            0,
            "a session is not a pinned transaction"
        );
        let (receipt, completion) = session
            .query(
                &query,
                "CREATE (a:Visible {p:1}), (b:Visible {p:2}), (a)-[:R]->(b)",
                &GqlParameters::new(),
            )
            .await
            .unwrap();
        // The first engine allocations on this fresh database: vertex
        // counters 1 (a, p=1) and 2 (b, p=2), edge counter 1.
        let a = crate::write_txn::engine_vertex(&keys(), 1);
        let b = crate::write_txn::engine_vertex(&keys(), 2);
        assert_eq!(receipt.steps()[0].created_vertices(), Some(&[a, b][..]));
        assert_eq!(
            receipt.steps()[0].created_edges(),
            Some(&[crate::write_txn::engine_edge(&keys(), 1)][..])
        );
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(basis.0 + 1)
            }
        );
        let prepared = session
            .prepare(
                &query,
                "MATCH (n:Visible) WHERE n.p=$key SET n.q=$value",
                &arguments(1, 10),
            )
            .unwrap();
        let frozen_calls = calls.load(Ordering::Relaxed);
        assert!(frozen_calls > 0);
        assert_eq!(prepared.parameter_schema().len(), 2);
        for (step, value) in [(2, 10), (3, 20)] {
            let (receipt, completion) = session
                .execute(&query, &prepared, &arguments(1, value))
                .await
                .unwrap();
            assert_eq!(receipt.steps()[0].mutation_targets(), Some(&[a][..]));
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(basis.0 + step)
                }
            );
            assert_eq!(calls.load(Ordering::Relaxed), frozen_calls);
            assert_eq!(txn.outstanding_obligations(), 0);
            assert!(!session.is_closed());
        }
        let debug = format!("{session:?} {prepared:?}");
        assert!(debug.contains("[REDACTED]"));
        for secret in ["host-branch", "host-graph", "Visible", "MATCH", "$value"] {
            assert!(!debug.contains(secret));
        }
        session.close();
        assert!(session.is_closed());
        session.close();
        drop(session);
        assert_eq!(
            db.vertex(a).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(20))]
        );
        assert_eq!(db.edges().unwrap().len(), 1);
        let expected = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            expected
        );
    });
}

#[test]
fn constructor_checks_namespace_branch_write_rights_and_expiry_without_catalog_calls() {
    lab(0xac02, |contexts| async move {
        let commit = contexts.commit();
        let txn = contexts.txn();
        for mode in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let issuer = authority();
            let other = Authority::new(
                AuthKey::from_seed(10102),
                if mode == 2 {
                    DatabaseSecurityNamespaceId([0xa3; 32])
                } else {
                    NS
                },
                "host-graph",
                SchemaEpoch(1),
                1,
            )
            .unwrap();
            let mut scope = grant();
            if mode == 0 {
                scope.rights = Rights::Read;
            }
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let calls = AtomicUsize::new(0);
            let error = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    if mode == 2 || mode == 4 {
                        &other
                    } else {
                        &issuer
                    },
                    &token,
                    if mode == 1 {
                        "wrong-route"
                    } else {
                        "host-branch"
                    },
                    |kind, name| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        symbols(kind, name)
                    },
                    R,
                    policy(),
                    64,
                    || if mode == 3 { 10_000 } else { NOW },
                )
                .unwrap_err();
            assert!(authorization(&error).is_some(), "mode={mode}: {error:?}");
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn write_only_session_cannot_prepare_selected_writes_or_fall_back_to_raw_reads() {
    lab(0xac03, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let mut scope = grant();
        scope.rights = Rights::Write;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        for text in ["MATCH (n:Visible) SET n.q=1", "MATCH (n:Visible) RETURN n"] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "host-branch",
                    symbols,
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            session
                .query(&query, "CREATE (n:Visible {p:1})", &GqlParameters::new())
                .await
                .unwrap();
            let error = session
                .prepare(&query, text, &GqlParameters::new())
                .unwrap_err();
            if text.contains(" SET ") {
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            } else {
                assert!(matches!(error, Fault::Binding(_)));
            }
            assert!(session.is_closed());
            let error = session
                .query(&query, "CREATE (n:Visible)", &GqlParameters::new())
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ExecutionStopped));
            drop(session);
            assert_eq!(db.vertices().unwrap().len(), 1);
            // The one committed CREATE took vertex counter 1 of this fresh
            // database; the refused second CREATE stopped before allocating.
            assert_eq!(
                db.vertex(crate::write_txn::engine_vertex(&keys(), 1))
                    .unwrap()
                    .unwrap()
                    .props,
                vec![(P, CanonicalScalar::Int(1))]
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn prepared_handle_cannot_cross_sessions_even_with_identical_database_and_credentials() {
    lab(0xac04, |contexts| async move {
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
                "host-branch",
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let prepared = first
            .prepare(
                &query,
                "CREATE (n:Visible {p:$key})",
                &GqlParameters::new().with_int64("key", 1).unwrap(),
            )
            .unwrap();
        drop(first);
        let calls = AtomicUsize::new(0);
        let mut second = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "host-branch",
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
            .execute(&query, &prepared, &GqlParameters::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::Program(GraphWriteProgramError::Program(
                GraphMutationProgramError::Preflight(WriteTxnError::AuthorizedMutationRefused)
            ))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(second.is_closed());
        drop(second);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        // The refused cross-session CREATE reserved nothing: the vertex
        // counter of this fresh database still issues its first identity.
        assert_eq!(
            db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                .unwrap(),
            ElementId::Vertex(crate::write_txn::engine_vertex(&keys(), 1))
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn clock_high_water_survives_successful_commands_and_credentials_remain_live() {
    lab(0xac05, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        for mode in 0..3 {
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let time = AtomicU64::new(NOW);
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "host-branch",
                    symbols,
                    R,
                    policy(),
                    64,
                    || time.load(Ordering::Relaxed),
                )
                .unwrap();
            time.store(200, Ordering::Relaxed);
            session
                .query(&query, "CREATE (n:Visible {p:1})", &GqlParameters::new())
                .await
                .unwrap();
            match mode {
                0 => time.store(150, Ordering::Relaxed),
                1 => time.store(10_000, Ordering::Relaxed),
                _ => {
                    assert!(issuer.retire());
                }
            }
            let error = session
                .query(&query, "CREATE (n:Visible {p:2})", &GqlParameters::new())
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(match mode {
                    0 => Error::ClockWentBackwards,
                    1 => Error::Expired,
                    _ => Error::AuthorityRetired,
                })
            );
            assert!(session.is_closed());
            drop(session);
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert_eq!(txn.outstanding_obligations(), 0);
            if mode == 0 {
                let result = db
                    .query_write_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "host-branch",
                        "CREATE (n:Visible {p:2})",
                        &GqlParameters::new(),
                        symbols,
                        R,
                        policy(),
                        || 150,
                    )
                    .await
                    .unwrap();
                assert!(matches!(result, QueryResult::Write { .. }));
                assert_eq!(db.vertices().unwrap().len(), 2);
            }
        }
    });
}

#[test]
fn forbidden_tail_and_host_ceilings_close_without_publishing_a_prefix() {
    lab(0xac06, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        for mode in 0..3 {
            let issuer = authority();
            let mut scope = grant();
            if mode == 1 {
                scope.limits.max_rows = 1;
            }
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(99),
                vec![L],
                vec![
                    (P, CanonicalScalar::Int(9)),
                    (SECRET, CanonicalScalar::Int(777)),
                ],
            );
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "host-branch",
                    symbols,
                    R,
                    policy(),
                    if mode == 2 { 1 } else { 64 },
                    || NOW,
                )
                .unwrap();
            let text = if mode == 0 {
                "MATCH (n:Visible) SET n.q=1; MATCH (n:Visible) SET n.secret=n.secret"
            } else {
                "CREATE (n:Visible {p:1}); CREATE (n:Visible {p:2})"
            };
            let error = session
                .query(&query, text, &GqlParameters::new())
                .await
                .unwrap_err();
            match mode {
                0 => assert_eq!(authorization(&error), Some(Error::ScopeDenied)),
                1 => assert_eq!(
                    authorization(&error),
                    Some(Error::LimitExceeded(LimitDimension::Rows))
                ),
                _ => assert!(matches!(
                    error,
                    Fault::BatchBinding(GraphWriteScriptBatchError::TooManyStatements {
                        limit: 1,
                        observed: 2
                    })
                )),
            }
            assert!(session.is_closed());
            drop(session);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn dropping_unpolled_commands_is_inert_but_callback_unwind_closes_the_session() {
    lab(0xac07, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for asynchronous in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let calls = AtomicUsize::new(0);
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "host-branch",
                    |_, _| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        panic!("injected resolver unwind")
                    },
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            let params = GqlParameters::new();
            drop(session.query(&query, "CREATE (n:Visible)", &params));
            assert!(!session.is_closed());
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            let panic = if asynchronous {
                let mut future = Box::pin(session.query(&query, "CREATE (n:Visible)", &params));
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    future.as_mut().poll(&mut cx)
                }));
                drop(future);
                result.is_err()
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(session.prepare(&query, "CREATE (n:Visible)", &params));
                }))
                .is_err()
            };
            assert!(panic, "the injected callback must execute");
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(session.is_closed());
            let error = session
                .prepare(&query, "CREATE (n:Visible)", &params)
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ExecutionStopped));
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            drop(session);
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn each_success_gets_one_per_execution_budget_not_a_lifetime_grant_or_phase_reset() {
    lab(0xac08, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let mut scope = grant();
        scope.limits.max_rows = 1;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut limits = policy();
        limits.max_created_vertices = 1;
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "host-branch",
                symbols,
                R,
                limits,
                64,
                || NOW,
            )
            .unwrap();
        for _ in 0..2 {
            session
                .query(&query, "CREATE (n:Visible)", &GqlParameters::new())
                .await
                .unwrap();
        }
        assert!(!session.is_closed());
        assert!(
            session
                .query(
                    &query,
                    "CREATE (a:Visible); CREATE (b:Visible)",
                    &GqlParameters::new()
                )
                .await
                .is_err()
        );
        assert!(session.is_closed());
        drop(session);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn bound_ingestion_reuses_graph_and_templates_across_more_than_sixty_four_steps() {
    lab(0xac11, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let calls = AtomicUsize::new(0);
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "host-branch",
                |kind, name| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    symbols(kind, name)
                },
                R,
                policy(),
                72,
                || NOW,
            )
            .unwrap();
        let template = session
            .prepare(
                &query,
                "MERGE (n:Visible {p:0}); MERGE (n:Visible {p:$key}) ON CREATE SET n.q=$value; \
             MATCH (a:Visible),(b:Visible) WHERE a.p=0 AND b.p=$key MERGE (a)-[:R]->(b)",
                &arguments(1, 11),
            )
            .unwrap();
        let args: Vec<_> = (1..=24).map(|key| arguments(key, key + 10)).collect();
        let batch = session.bind_batch(&query, &template, &args).unwrap();
        assert_eq!(batch.argument_sets(), 24);
        assert_eq!(batch.location(71).unwrap().argument_set, 23);
        assert_eq!(batch.location(71).unwrap().statement, 2);
        assert!(batch.location(72).is_none());
        assert!(format!("{batch:?}").contains("[REDACTED]"));
        assert!(!format!("{batch:?}").contains("Visible"));
        let resolved = calls.load(Ordering::Relaxed);
        assert_eq!(txn.outstanding_obligations(), 0);
        let (receipt, completion) = session.execute_bound_batch(&query, &batch).await.unwrap();
        assert_eq!(receipt.stats().completed_statements, 72);
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (25, 24)
        );
        assert_eq!(receipt.stats().mutation_effects, 24);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(basis.0 + 1)
            }
        );
        // Record 0's first MERGE creates the shared p:0 vertex from vertex
        // counter 1; every later record matches it. Each record's
        // relationship MERGE is the only edge creation, so record r takes
        // edge counter r + 1.
        let shared = crate::write_txn::engine_vertex(&keys(), 1);
        for record in 0..24 {
            let steps = batch.record_receipts(&receipt, record).unwrap();
            assert_eq!(steps.len(), 3);
            assert_eq!(steps[0].merged_vertex().unwrap().vertex(), shared);
            let edge = crate::write_txn::engine_edge(&keys(), record as u64 + 1);
            assert_eq!(steps[2].created_edges(), Some(&[edge][..]));
        }
        assert!(batch.record_receipts(&receipt, 24).is_none());
        let (repeat, completion) = session.execute_bound_batch(&query, &batch).await.unwrap();
        assert_eq!(
            (
                repeat.stats().created_vertices,
                repeat.stats().created_edges
            ),
            (0, 0)
        );
        assert_eq!(repeat.stats().mutation_effects, 0);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        let (stats, completion) = session
            .execute_bound_batch_stats(&query, &batch)
            .await
            .unwrap();
        assert_eq!(stats, repeat.stats());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(calls.load(Ordering::Relaxed), resolved);
        drop(session);
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        assert_eq!(db.vertices().unwrap().len(), 25);
        assert_eq!(db.edges().unwrap().len(), 24);
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn bound_batches_do_not_outlive_their_owner_or_live_credentials() {
    lab(0xac15, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        for mode in 0..3 {
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let time = AtomicU64::new(NOW);
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "host-branch",
                    symbols,
                    R,
                    policy(),
                    64,
                    || time.load(Ordering::Relaxed),
                )
                .unwrap();
            let template = session
                .prepare(
                    &query,
                    "CREATE (n:Visible {p:$key, q:$value})",
                    &arguments(1, 10),
                )
                .unwrap();
            let batch = session
                .bind_batch(&query, &template, &[arguments(1, 10), arguments(2, 20)])
                .unwrap();
            if mode == 0 {
                drop(session);
                let mut other = db
                    .authorized_write_session(
                        &txn,
                        &commit,
                        &issuer,
                        &token,
                        "host-branch",
                        symbols,
                        R,
                        policy(),
                        64,
                        || NOW,
                    )
                    .unwrap();
                let error = other.execute_bound_batch(&query, &batch).await.unwrap_err();
                assert!(matches!(
                    error,
                    Fault::Program(GraphWriteProgramError::Program(
                        GraphMutationProgramError::Preflight(
                            WriteTxnError::AuthorizedMutationRefused
                        )
                    ))
                ));
                assert!(other.is_closed());
                drop(other);
            } else {
                if mode == 1 {
                    time.store(10_000, Ordering::Relaxed);
                } else {
                    issuer.retire();
                }
                let error = session
                    .execute_bound_batch(&query, &batch)
                    .await
                    .unwrap_err();
                assert_eq!(
                    authorization(&error),
                    Some(if mode == 1 {
                        Error::Expired
                    } else {
                        Error::AuthorityRetired
                    })
                );
                assert!(session.is_closed());
                drop(session);
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            // Every refusal above precedes reservation: the vertex counter of
            // this fresh database still issues its first identity.
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(crate::write_txn::engine_vertex(&keys(), 1))
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn batch_binding_and_execution_share_the_exact_signed_work_allowance() {
    lab(0xac14, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let args: Vec<_> = (1..=3).map(|key| arguments(key, key + 10)).collect();
        let mut floors = Vec::new();
        for phase in 0..3 {
            let (mut low, mut high) = (0_u64, 16384_u64);
            while low < high {
                let middle = low + (high - low) / 2;
                let mut scope = grant();
                scope.limits.max_work = middle;
                scope.limits.max_rows = 0;
                let token = issuer.issue_at(&scope, NOW).unwrap();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                let mut session = db
                    .authorized_write_session(
                        &txn,
                        &commit,
                        &issuer,
                        &token,
                        "host-branch",
                        symbols,
                        R,
                        policy(),
                        64,
                        || NOW,
                    )
                    .unwrap();
                let result = async {
                    if phase == 2 {
                        return session.query_batch_stats(&query,
                            "CREATE (n:Visible {p:$key}); MATCH (n:Visible) WHERE n.p=$key SET n.q=$value",
                            &args,
                        ).await;
                    }
                    let template = session.prepare(&query,
                        "CREATE (n:Visible {p:$key}); MATCH (n:Visible) WHERE n.p=$key SET n.q=$value",
                        &args[0],
                    )?;
                    if phase == 1 {
                        let batch = session.bind_batch(&query, &template, &args)?;
                        session.execute_bound_batch_stats(&query, &batch).await
                    } else {
                        session.execute_batch_stats(&query, &template, &args).await
                    }
                }.await;
                let success = match result {
                    Ok((stats, _)) => {
                        assert_eq!(stats.created_vertices, 3);
                        true
                    }
                    Err(error) => {
                        assert_eq!(
                            authorization(&error),
                            Some(Error::LimitExceeded(LimitDimension::Work))
                        );
                        assert!(session.is_closed());
                        false
                    }
                };
                drop(session);
                if success {
                    high = middle;
                } else {
                    low = middle + 1;
                    assert_eq!(db.frontier().unwrap(), before);
                    assert!(db.vertices().unwrap().is_empty());
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(low > 0 && low < 16384);
            floors.push(low);
        }
        assert_eq!(floors[0], floors[1] + 6 + 3 + 1);
        assert!(
            floors[2] > floors[0],
            "text-batch preparation must consume the same allowance as binding and execution"
        );
    });
}

#[test]
fn text_batches_merge_record_major_with_zero_output_rows_and_reopen_exactly() {
    lab(0xac15, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut hidden = WriteBatch::new(R);
        hidden.create_vertex(
            VId(77),
            vec![LabelId(2)],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(999))],
        );
        db.write(&commit, hidden).await.unwrap();
        let before = db.frontier().unwrap();
        let hidden = db.vertex(VId(77)).unwrap().unwrap();
        let issuer = authority();
        let mut scope = grant();
        scope.limits.max_rows = 0;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "host-branch",
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let (stats, completion) = session
            .query_batch_stats(
                &query,
                "MERGE (n:Visible {p:$key}) ON CREATE SET n.q=$value ON MATCH SET n.q=n.q+$value",
                &[arguments(1, 10), arguments(1, 20), arguments(2, 7)],
            )
            .await
            .unwrap();
        assert_eq!(stats.completed_statements, 3);
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(before.0 + 1)
            }
        );
        assert!(!session.is_closed());
        drop(session);
        assert_eq!(db.vertex(VId(77)).unwrap().unwrap(), hidden);
        let visible: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| vertex.labels.contains(&L))
            .map(|vertex| vertex.props)
            .collect();
        assert_eq!(visible.len(), 2);
        assert!(visible.contains(&vec![
            (P, CanonicalScalar::Int(1)),
            (Q, CanonicalScalar::Int(30))
        ]));
        assert!(visible.contains(&vec![
            (P, CanonicalScalar::Int(2)),
            (Q, CanonicalScalar::Int(7))
        ]));
        let expected = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            expected
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn text_batches_refuse_bad_final_arguments_returning_and_execution_without_a_prefix() {
    lab(0xac16, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let one = |value| GqlParameters::new().with_int64("value", value).unwrap();
        for (case, text, args, limit) in [
            (
                0,
                "CREATE (:Visible {p:$value})",
                vec![one(1), GqlParameters::new()],
                64,
            ),
            (
                1,
                "CREATE (:Visible {p:10/$value})",
                vec![one(2), one(0)],
                64,
            ),
            (
                2,
                "CREATE (n:Visible {p:$value}) RETURN 1/0 AS invalid LIMIT 0",
                vec![one(1)],
                64,
            ),
            (
                3,
                "CREATE (:Visible {p:$value}); CREATE (:Visible {q:$value})",
                vec![one(1), one(2)],
                3,
            ),
            (4, "CREATE (:Visible {p:$value})", vec![], 64),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &commit,
                    &issuer,
                    &token,
                    "host-branch",
                    symbols,
                    R,
                    policy(),
                    limit,
                    || NOW,
                )
                .unwrap();
            let error = session
                .query_batch_stats(&query, text, &args)
                .await
                .unwrap_err();
            assert!(session.is_closed(), "case {case}");
            match case {
                0 => assert!(matches!(
                    error,
                    Fault::BatchBinding(GraphWriteScriptBatchError::Arguments {
                        argument_set: 1,
                        ..
                    })
                )),
                1 => assert!(matches!(error, Fault::BatchProgram { .. })),
                2 => assert!(matches!(error, Fault::Binding(_))),
                3 => assert!(matches!(
                    error,
                    Fault::BatchBinding(GraphWriteScriptBatchError::TooManyStatements {
                        limit: 3,
                        observed: 4
                    })
                )),
                4 => assert!(matches!(
                    error,
                    Fault::BatchBinding(GraphWriteScriptBatchError::Empty)
                )),
                _ => unreachable!(),
            }
            drop(session);
            assert_eq!(db.frontier().unwrap(), before, "case {case}");
            assert!(db.vertices().unwrap().is_empty(), "case {case}");
            assert!(db.edges().unwrap().is_empty(), "case {case}");
            assert_eq!(txn.outstanding_obligations(), 0, "case {case}");
        }
    });
}
