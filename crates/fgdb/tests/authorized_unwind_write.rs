//! Native UNWIND ingress shares real Warden permits and one Chronicle completion.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, IdentityPermutation, MemVfs, QueryResult, WriteBatch, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::unwind_write::{
    GraphUnwindBindEvent, GraphUnwindRowError, GraphUnwindWriteError, GraphUnwindWriteText,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    GraphWriteProgramReceipt, GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

type Fault = GraphWriteScriptExecutionError<WriteTxnError, WriteTxnError, WriteTxnError>;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xa6; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;
const COUNTER: &str = "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) \
    ON CREATE SET n.q=0 SET n.q=n.q+row.q";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa5; 32], NS, [0xa7; 32])
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
    Authority::new(AuthKey::from_seed(0xa611), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: EXPIRES,
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
fn row(p: i64, q: CanonicalScalar) -> GraphValue {
    GraphValue::map(vec![
        ("p".into(), GraphValue::Scalar(CanonicalScalar::Int(p))),
        ("q".into(), GraphValue::Scalar(q)),
    ])
    .unwrap()
}
fn arguments(values: &[(i64, i64)]) -> GqlParameters {
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
fn receipt(result: QueryResult) -> (GraphWriteProgramReceipt, EmbeddedTxnCompletion) {
    match result {
        QueryResult::Write {
            receipt,
            completion: Some(completion),
        } => (receipt, completion),
        other => panic!("expected exactly one completed native receipt: {other:?}"),
    }
}
fn authorization(error: &Fault) -> Option<Error> {
    let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
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
fn repeated_vertex_keys_share_one_authorized_commit_and_survive_reopen() {
    lab(0xa611_0001, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let args = arguments(&[(1, 2), (2, 4), (1, 3)]);
        let original = args.canonical_bytes();
        let (result, completion) = receipt(
            db.query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                COUNTER,
                &args,
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap(),
        );
        assert_eq!(result.stats().completed_statements, 3);
        assert_eq!(result.stats().created_vertices, 2);
        let first = result.steps()[0].merged_vertex().unwrap().vertex();
        let second = result.steps()[1].merged_vertex().unwrap().vertex();
        assert_eq!(result.steps()[2].merged_vertex().unwrap().vertex(), first);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
        );
        assert!(pinned.vertex(first).unwrap().is_none());
        assert_eq!(args.canonical_bytes(), original);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
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
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn relationship_batches_reuse_masked_native_merge_and_admit_relation_scope() {
    lab(0xa611_0002, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(2))]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let issuer = authority();
        let text = "UNWIND $rows AS row MATCH (a:Visible {p:1}),(b:Visible {p:row.p}) \
            MERGE (a)-[e:R]->(b) ON CREATE SET e.q=0 SET e.q=e.q+row.q";
        let args = arguments(&[(2, 2), (2, 3)]);
        let mut denied = grant();
        denied.relations = Scope::only([RelationId(2)]);
        let token = issuer.issue_at(&denied, NOW).unwrap();
        let error = db
            .query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                text,
                &args,
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert!(
            matches!(error, Fault::Program(_)),
            "definition admission precedes execution"
        );
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let (result, _) = receipt(
            db.query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                text,
                &args,
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap(),
        );
        assert_eq!(result.stats().created_edges, 1);
        assert_eq!(result.stats().completed_statements, 2);
        let edge = result.steps()[0].merged_edge().unwrap().edge().unwrap();
        assert_eq!(result.steps()[1].merged_edge().unwrap().edge(), Some(edge));
        assert_eq!(
            db.edge(edge).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(5).into())]
        );
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn authority_and_read_rights_precede_missing_row_arguments_and_catalog() {
    lab(0xa611_0003, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let issuer = authority();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut scope = grant();
            scope.rights = match mode {
                0 => Rights::Read,
                1 => Rights::Write,
                _ => Rights::ReadWrite,
            };
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let calls = AtomicUsize::new(0);
            let error = db
                .query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    if mode == 2 { "other" } else { "main" },
                    COUNTER,
                    &GqlParameters::new(),
                    |kind: GraphSymbolKind, name: &str| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        symbols(kind, name)
                    },
                    R,
                    policy(),
                    || if mode == 3 { EXPIRES + 1 } else { NOW },
                )
                .await
                .unwrap_err();
            assert!(authorization(&error).is_some(), "mode {mode}: {error:?}");
            if mode <= 1 {
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            }
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            if mode == 1 {
                // A write-only grant is usable, just not for a reading UNWIND.
                db.query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    "CREATE (:Visible {p:1})",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
                assert_eq!(db.vertices().unwrap().len(), 1);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn malformed_tail_and_row_limit_refuse_before_resolution_or_identity_reservation() {
    lab(0xa611_0004, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let args = GqlParameters::new()
            .with_list(
                "rows",
                vec![
                    row(1, CanonicalScalar::Int(2)),
                    row(2, CanonicalScalar::Bool(true)),
                ],
            )
            .unwrap();
        let error = db
            .query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                COUNTER,
                &args,
                |_: GraphSymbolKind, _: &str| {
                    panic!("late type refusal must precede catalog access")
                },
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::UnwindBinding(GraphUnwindWriteError::Row {
                row: 1,
                kind: GraphUnwindRowError::IncompatibleFieldTypes,
                ..
            })
        ));
        let error = db
            .query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                COUNTER,
                &arguments(&vec![(1, 1); 65]),
                |_: GraphSymbolKind, _: &str| panic!("row cap precedes catalog"),
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::UnwindBinding(GraphUnwindWriteError::TooManyRows {
                limit: 64,
                observed: 65,
            })
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        // Neither refusal took a counter: this handle's first engine
        // allocation is still vertex counter 1.
        assert_eq!(
            db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                .unwrap(),
            ElementId::Vertex(engine_vertex(1)),
            "binding must not reserve graph IDs"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn late_overflow_action_creation_and_signed_row_limits_never_publish_a_prefix() {
    lab(0xa611_0005, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let issuer = authority();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut scope = grant();
            let mut allowance = policy();
            let args = match mode {
                0 => arguments(&[(1, 2), (1, i64::MAX)]),
                1 => {
                    allowance.mutations.max_effects = 2;
                    arguments(&[(1, 2), (1, 3)])
                }
                2 => {
                    allowance.max_created_vertices = 1;
                    arguments(&[(1, 2), (2, 3)])
                }
                _ => {
                    scope.limits.max_rows = 1;
                    arguments(&[(1, 2), (1, 3)])
                }
            };
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    COUNTER,
                    &args,
                    symbols,
                    R,
                    allowance,
                    || NOW,
                )
                .await
                .unwrap_err();
            if mode == 3 {
                assert!(authorization(&error).is_some(), "{error:?}");
            }
            let Fault::BatchProgram {
                location: Some(location),
                ..
            } = error
            else {
                panic!("mode {mode}: expected an executed-record coordinate")
            };
            assert_eq!(location.argument_set, 1);
            assert_eq!(location.statement, 0);
            assert_eq!(location.span, 0..COUNTER.len());
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn expiry_after_catalog_is_a_live_refusal_not_an_unknown_symbol_or_partial_write() {
    lab(0xa611_0006, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let now = AtomicU64::new(NOW);
        let calls = AtomicUsize::new(0);
        let error = db
            .query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                COUNTER,
                &arguments(&[(1, 2)]),
                |_: GraphSymbolKind, _: &str| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    now.store(EXPIRES + 1, Ordering::Relaxed);
                    None
                },
                R,
                policy(),
                || now.load(Ordering::Relaxed),
            )
            .await
            .unwrap_err();
        assert!(
            authorization(&error).is_some(),
            "expiry cannot become a parser error: {error:?}"
        );
        assert!(
            matches!(error, Fault::Program(_)),
            "no execution record is fabricated"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn hidden_fields_stay_masked_and_original_forbidden_writes_cannot_escape() {
    lab(0xa611_0007, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut first = None;
        for payload in ["x".to_owned(), "hidden".repeat(700)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let secret = CanonicalScalar::ucs_basic_text(&payload).unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![L],
                vec![
                    (P, CanonicalScalar::Int(1)),
                    (Q, CanonicalScalar::Int(10)),
                    (SECRET, secret.clone()),
                ],
            );
            seed.create_vertex(VId(99), vec![HIDDEN], vec![(P, CanonicalScalar::Int(1))]);
            db.write(&commit, seed).await.unwrap();
            let hidden = db.vertex(VId(99)).unwrap();
            let clock_calls = AtomicUsize::new(0);
            let result = db
                .query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) \
                 SET n.q=n.q+COALESCE(n.secret,0)+row.q",
                    &arguments(&[(1, 2), (1, 3)]),
                    symbols,
                    R,
                    policy(),
                    || {
                        clock_calls.fetch_add(1, Ordering::Relaxed);
                        NOW
                    },
                )
                .await
                .unwrap();
            let trace = (result, clock_calls.load(Ordering::Relaxed));
            if let Some(expected) = &first {
                assert_eq!(&trace, expected);
            } else {
                first = Some(trace);
            }
            let stored = db.vertex(VId(1)).unwrap().unwrap();
            assert_eq!(
                stored.props,
                vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (Q, CanonicalScalar::Int(15).into()),
                    (SECRET, secret.into()),
                ]
            );
            assert_eq!(db.vertex(VId(99)).unwrap(), hidden);
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let error = db
                .query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) \
                 ON CREATE SET n.secret=row.q SET n.q=5",
                    &arguments(&[(2, 3)]),
                    symbols,
                    R,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn binding_exhaustion_does_not_refresh_the_execution_permit() {
    lab(0xa611_0008, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let issuer = authority();
        let text = "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) SET n.q=row.q";
        let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(4096)).unwrap();
        let args = GqlParameters::new()
            .with_list("rows", vec![row(1, payload.clone()), row(1, payload)])
            .unwrap();
        let mut binding_work = 0;
        let bound = GraphUnwindWriteText::parse(text)
            .unwrap()
            .bind_with_limit_controlled(&args, R, 64, symbols, |event| {
                if let GraphUnwindBindEvent::Work(units) = event {
                    binding_work += units;
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        let mut scope = grant();
        // Original-text admission, post-classification check, then exactly the
        // controlled binding bill. There is no spare unit for program admission.
        scope.limits.max_work = text.len() as u64 + 1 + binding_work;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let error = db
            .query_write_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                text,
                &args,
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(authorization(&error).is_some(), "{error:?}");
        assert!(
            matches!(error, Fault::BatchProgram { location: None, .. }),
            "binding must finish and the SAME exhausted permit must refuse execution"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        // The same grant can execute the already-bound program. This rules out
        // a quota too small for the write itself as the reason for refusal.
        let (receipt, _) = db
            .execute_graph_write_program_returning_authorized(
                &txcx,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                bound.program(),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

mod computed_map_input {
    use super::*;
    use fgdb_gql::{GraphSymbolResolver, ReverseSymbolCatalog};

    struct Catalog;
    impl GraphSymbolResolver for Catalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }

        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::default();
            catalog.insert_label(L, "Visible");
            catalog.insert_relation(R, "R");
            catalog.insert_property(P, "p");
            catalog.insert_property(Q, "q");
            catalog.insert_property(SECRET, "secret");
            Some(catalog)
        }
    }

    const UPDATE: &str = "UNWIND $rows AS row MATCH (n:Visible {p:row.p}) SET n += row.props";

    fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
        GraphValue::map(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
        .unwrap()
    }

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn patches(rows: Vec<(i64, GraphValue)>) -> GqlParameters {
        GqlParameters::new()
            .with_list(
                "rows",
                rows.into_iter()
                    .map(|(p, props)| map(vec![("p", int(p)), ("props", props)]))
                    .collect(),
            )
            .unwrap()
    }

    async fn fixture(commit: &fgdb_types::CommitCx) -> Database<MemVfs> {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(10))],
        );
        seed.create_vertex(
            VId(2),
            vec![L],
            vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(20))],
        );
        db.write(commit, seed).await.unwrap();
        db
    }

    #[test]
    fn heterogeneous_computed_maps_freeze_the_complete_match_bag_and_commit_once() {
        lab(0xa611_0401, |contexts| async move {
            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = fixture(&commit).await;
            let before = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let arguments = patches(vec![
                (1, map(vec![("p", int(2))])),
                (2, map(vec![("q", int(77))])),
            ]);
            let (result, completion) = receipt(
                db.query_write_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    UPDATE,
                    &arguments,
                    Catalog,
                    R,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap(),
            );
            assert_eq!(result.stats().completed_statements, 1);
            assert_eq!(result.stats().mutation_effects, 2);
            // The first map changes the second row's MATCH key. Its newly
            // matching vertex must not enter that row's frozen selection, and
            // the missing q key in its map must retain the original value.
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(2).into()),
                    (Q, CanonicalScalar::Int(10).into()),
                ],
            );
            assert_eq!(
                db.vertex(VId(2)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(2).into()),
                    (Q, CanonicalScalar::Int(77).into()),
                ],
            );
            assert_eq!(
                pinned.vertex(VId(1)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (Q, CanonicalScalar::Int(10).into()),
                ],
            );
            let after = db.frontier().unwrap();
            assert_eq!(after.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { commit_seq: after },
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn late_invalid_or_denied_map_discards_all_earlier_map_effects() {
        lab(0xa611_0402, |contexts| async move {
            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = fixture(&commit).await;
            let before = db.frontier().unwrap();
            let vertices = db.vertices().unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            for (invalid, denied) in [
                (GraphValue::Scalar(CanonicalScalar::Null), false),
                (map(vec![("q", map(vec![("bad", int(1))]))]), false),
                (map(vec![("secret", int(1))]), true),
            ] {
                let arguments = patches(vec![(1, map(vec![("q", int(99))])), (2, invalid)]);
                let error = db
                    .query_write_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        UPDATE,
                        &arguments,
                        Catalog,
                        R,
                        policy(),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(error, Fault::Program(_)), "{error:?}");
                if denied {
                    assert_eq!(authorization(&error), Some(Error::ScopeDenied));
                }
                assert_eq!(db.frontier().unwrap(), before);
                assert_eq!(db.vertices().unwrap(), vertices);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn computed_map_read_rights_precede_arguments_and_catalog() {
        lab(0xa611_0403, |contexts| async move {
            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = fixture(&commit).await;
            let before = db.frontier().unwrap();
            let issuer = authority();
            let mut write_only = grant();
            write_only.rights = Rights::Write;
            let token = issuer.issue_at(&write_only, NOW).unwrap();
            let error = db
                .query_write_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    UPDATE,
                    &GqlParameters::new(),
                    |_: GraphSymbolKind, _: &str| panic!("read admission precedes the catalog"),
                    R,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }
}

mod frozen_updates {
    use super::*;
    use fgdb::VertexRow;
    use fgdb_gql::{
        GqlQueryError, GraphMutationError, GraphMutationProgramError, GraphWriteProgramError,
        PreparedGraphWriteProgram,
    };
    use fgdb_types::CommitCx;
    use fgdb_warden::LimitDimension;

    type ProgramFault = GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;
    const UPDATE: &str = "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.q=row.q";

    fn program(text: &str, values: &[(i64, i64)]) -> PreparedGraphWriteProgram {
        let bound = GraphUnwindWriteText::parse(text)
            .unwrap()
            .bind(&arguments(values), R, symbols)
            .unwrap();
        assert!(bound.program().freezes_vertex_update_input());
        bound.program().clone()
    }

    fn sequential(program: &PreparedGraphWriteProgram) -> PreparedGraphWriteProgram {
        let program = PreparedGraphWriteProgram::prepare(program.statements().to_vec()).unwrap();
        assert!(!program.freezes_vertex_update_input());
        program
    }

    async fn fixture(commit: &CommitCx, hidden: bool) -> Database<MemVfs> {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        // Keep the visible source fixed as the number of update statements
        // changes. Source admission is then linear in statements too, so it
        // cannot conceal quadratic staging in the signed-work law below.
        for id in 1..=32 {
            let mut props = vec![(P, CanonicalScalar::Int(id)), (Q, CanonicalScalar::Int(0))];
            if hidden {
                props.push((SECRET, CanonicalScalar::Int(7)));
            }
            batch.create_vertex(VId(id as u128), vec![L], props);
        }
        if hidden {
            for id in 1..=8 {
                // These vertices match the unlabelled privileged selection's
                // property predicate. Only capability admission removes them.
                batch.create_vertex(
                    VId(100 + id as u128),
                    vec![HIDDEN],
                    vec![
                        (P, CanonicalScalar::Int(id)),
                        (Q, CanonicalScalar::Int(777)),
                        (
                            SECRET,
                            CanonicalScalar::ucs_basic_text(&"private".repeat(80)).unwrap(),
                        ),
                    ],
                );
            }
        }
        db.write(commit, batch).await.unwrap();
        db
    }

    fn authorization(error: &ProgramFault) -> Option<Error> {
        let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(error);
        while let Some(error) = cause {
            if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>()
            {
                return Some(*error);
            }
            cause = error.source();
        }
        None
    }

    async fn execute(
        contexts: &PurposeContexts,
        program: &PreparedGraphWriteProgram,
        scope: Grant,
        hidden: bool,
    ) -> Result<(GraphWriteProgramReceipt, Vec<VertexRow>), ProgramFault> {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut db = fixture(&commit, hidden).await;
        let before = db.vertices().unwrap();
        let frontier = db.frontier().unwrap();
        let result = db
            .execute_graph_write_program_returning_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                program,
                policy(),
                || NOW,
            )
            .await;
        assert_eq!(txn.outstanding_obligations(), 0);
        match result {
            Ok((receipt, completion)) => {
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: fgdb_types::CommitSeq(frontier.0 + 1)
                    }
                );
                Ok((receipt, db.vertices().unwrap()))
            }
            Err(error) => {
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(db.vertices().unwrap(), before);
                Err(error)
            }
        }
    }

    async fn minimum_work(
        contexts: &PurposeContexts,
        program: &PreparedGraphWriteProgram,
        hidden: bool,
    ) -> u64 {
        let mut low = 0;
        let mut high = grant().limits.max_work;
        execute(contexts, program, grant(), hidden).await.unwrap();
        while low < high {
            let middle = low + (high - low) / 2;
            let mut scope = grant();
            scope.limits.max_work = middle;
            match execute(contexts, program, scope, hidden).await {
                Ok(_) => high = middle,
                Err(error) => {
                    assert_eq!(
                        authorization(&error),
                        Some(Error::LimitExceeded(LimitDimension::Work)),
                        "{error:?}"
                    );
                    low = middle + 1;
                }
            }
        }
        low
    }

    fn assert_keyed_selection_receipts(
        frozen: &GraphWriteProgramReceipt,
        sequential: &GraphWriteProgramReceipt,
        statements: u64,
    ) {
        assert_eq!(frozen.steps(), sequential.steps());
        let a = frozen.stats();
        let b = sequential.stats();
        assert_eq!(
            (
                a.completed_statements,
                a.selection.result_rows,
                a.target_vertex_visits,
                a.target_edge_visits,
                a.mutation_effects,
                a.created_vertices,
                a.created_edges,
            ),
            (
                b.completed_statements,
                b.selection.result_rows,
                b.target_vertex_visits,
                b.target_edge_visits,
                b.mutation_effects,
                b.created_vertices,
                b.created_edges,
            )
        );
        // Existing source choice, independent of the staging optimization:
        // every frozen selection uses the committed equality index. After its
        // first statement, ordinary execution admits the whole visible sparse
        // overlay. The fixture has exactly 32 visible vertices in both cases.
        assert_eq!(a.selection.snapshot_records, statements);
        assert_eq!(b.selection.snapshot_records, 1 + 32 * (statements - 1));
        assert!(
            a.evaluator.work_units < b.evaluator.work_units,
            "{a:?}; {b:?}"
        );
        assert!(
            a.evaluator.scratch_entries < b.evaluator.scratch_entries,
            "{a:?}; {b:?}"
        );
    }

    #[test]
    fn disjoint_updates_preserve_native_receipts_and_hidden_fields() {
        lab(0xa611_0101, |contexts| async move {
            // Two fields on one target are still checked in order inside a
            // statement. Rewriting its unchanged key keeps the independent
            // sequential program a valid comparison for frozen selection.
            for (keyed, text) in [
                (
                    true,
                    "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.q=row.q,n.p=row.p",
                ),
                // The equivalent range predicate is outside keyed_writes'
                // equality-only profile. Both executions admit all 32 visible
                // vertices, so the FULL receipts, including native resource
                // counters, must agree as well as the graph and identities.
                (
                    false,
                    "UNWIND $rows AS row MATCH (n) \
                         WHERE n.p >= row.p AND n.p <= row.p SET n.q=row.q,n.p=row.p",
                ),
            ] {
                let program = program(text, &[(1, 11), (2, 22), (3, 33)]);
                let ordinary = sequential(&program);
                let mut receipts = Vec::new();
                for hidden in [false, true] {
                    let (actual, graph) =
                        execute(&contexts, &program, grant(), hidden).await.unwrap();
                    let (expected, expected_graph) = execute(&contexts, &ordinary, grant(), hidden)
                        .await
                        .unwrap();
                    assert_eq!(graph, expected_graph);
                    if keyed {
                        assert_keyed_selection_receipts(&actual, &expected, 3);
                    } else {
                        assert_eq!(actual, expected);
                    }
                    assert_eq!(actual.stats().mutation_effects, 6);
                    for (at, step) in actual.steps().iter().enumerate() {
                        assert_eq!(
                            step.mutation_targets(),
                            Some([VId(at as u128 + 1)].as_slice())
                        );
                    }
                    for vertex in &graph {
                        if vertex.labels.contains(&HIDDEN) {
                            assert_eq!(vertex.props[1].1, CanonicalScalar::Int(777).into());
                        } else if hidden {
                            assert!(
                                vertex
                                    .props
                                    .contains(&(SECRET, CanonicalScalar::Int(7).into()))
                            );
                        }
                    }
                    receipts.push(actual);
                }
                // Hidden data may change neither semantic nor resource facts.
                assert_eq!(receipts[0], receipts[1]);
            }
        });
    }

    #[test]
    fn closed_collection_rekeys_keep_authorization_and_hidden_data_independence() {
        lab(0xa611_0105, |contexts| async move {
            let text = "UNWIND $rows AS row MATCH (n {p:row.p}) \
                SET n.p=row.q, n.q=[x IN [row.p,row.q] | x + 10]";
            let frozen = program(text, &[(1, 2), (2, 1), (1, 3)]);
            let mut receipts = Vec::new();
            for hidden in [false, true] {
                let (receipt, graph) = execute(&contexts, &frozen, grant(), hidden).await.unwrap();
                assert_eq!(receipt.stats().completed_statements, 3);
                assert_eq!(receipt.stats().mutation_effects, 6);
                for (id, before, after) in [(1, 1, 3), (2, 2, 1)] {
                    let vertex = graph.iter().find(|vertex| vertex.vid == VId(id)).unwrap();
                    let list = GraphValue::List(
                        vec![
                            GraphValue::Scalar(CanonicalScalar::Int(before + 10)),
                            GraphValue::Scalar(CanonicalScalar::Int(after + 10)),
                        ]
                        .into_boxed_slice(),
                    )
                    .to_property_value()
                    .unwrap();
                    assert_eq!(vertex.props[0].1, CanonicalScalar::Int(after).into());
                    assert_eq!(vertex.props[1].1, list);
                    if hidden {
                        assert!(
                            vertex
                                .props
                                .contains(&(SECRET, CanonicalScalar::Int(7).into()))
                        );
                    }
                }
                for vertex in graph
                    .iter()
                    .filter(|vertex| vertex.labels.contains(&HIDDEN))
                {
                    assert_eq!(vertex.props[1].1, CanonicalScalar::Int(777).into());
                }
                receipts.push(receipt);
            }
            assert_eq!(
                receipts[0], receipts[1],
                "hidden data changes neither rows nor metering"
            );

            let forbidden = program(
                "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.p=row.q, n.secret=[row.q]",
                &[(1, 2), (2, 1)],
            );
            assert!(
                authorization(
                    &execute(&contexts, &forbidden, grant(), true)
                        .await
                        .unwrap_err()
                )
                .is_some()
            );

            let fails_late = program(
                "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.p=row.q, n.q=[10 / row.q]",
                &[(1, 2), (2, 0)],
            );
            let error = execute(&contexts, &fails_late, grant(), true)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    ProgramFault::Program(GraphMutationProgramError::Statement {
                        statement: 1,
                        ..
                    })
                ),
                "the second row retains its expression failure: {error:?}"
            );
        });
    }

    #[test]
    fn disjoint_collection_updates_use_bounded_candidate_work() {
        lab(0xa611_0106, |contexts| async move {
            let mut counters = Vec::new();
            for count in [8, 16, 32] {
                let values: Vec<_> = (1..=count).map(|id| (id, id + 100)).collect();
                let program = program(
                    "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.q=[row.p,row.q]",
                    &values,
                );
                let (receipt, actual) = execute(&contexts, &program, grant(), false).await.unwrap();
                let (ordinary, expected) =
                    execute(&contexts, &sequential(&program), grant(), false)
                        .await
                        .unwrap();
                assert_eq!(actual, expected);
                assert_keyed_selection_receipts(&receipt, &ordinary, count as u64);
                counters.push(receipt.stats().evaluator.work_units);
            }
            assert!(
                counters
                    .windows(2)
                    .all(|pair| pair[1] > pair[0] && pair[1] <= pair[0] * 3),
                "doubling a disjoint collection batch must stay below quadratic work: {counters:?}"
            );
        });
    }

    #[test]
    fn disjoint_update_signed_work_is_linear_and_hidden_data_independent() {
        lab(0xa611_0102, |contexts| async move {
            let mut thresholds = Vec::new();
            for count in [8, 16, 32] {
                let values: Vec<_> = (1..=count).map(|id| (id, id + 100)).collect();
                let program = program(UPDATE, &values);
                thresholds.push(minimum_work(&contexts, &program, false).await);
            }
            assert_eq!(
                thresholds[2] - thresholds[1],
                2 * (thresholds[1] - thresholds[0]),
                "a fixed visible source and disjoint updates must have affine signed work"
            );
            let program = program(
                UPDATE,
                &(1..=32).map(|id| (id, id + 100)).collect::<Vec<_>>(),
            );
            assert_eq!(minimum_work(&contexts, &program, true).await, thresholds[2]);
            let mut exact = grant();
            exact.limits.max_work = thresholds[2];
            execute(&contexts, &program, exact.clone(), false)
                .await
                .unwrap();
            let error = execute(&contexts, &sequential(&program), exact, false)
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::LimitExceeded(LimitDimension::Work))
            );
        });
    }

    #[test]
    fn forbidden_tail_and_receipt_quota_abort_all_independent_updates() {
        lab(0xa611_0103, |contexts| async move {
            // The first optional target is absent. The second reaches a
            // forbidden no-op (secret already equals 7) after an allowed row.
            let forbidden = program(
                "UNWIND $rows AS row MATCH (n {p:row.p}) \
                 OPTIONAL MATCH (m {p:row.q}) SET n.q=row.q,m.secret=7",
                &[(1, 99), (2, 3)],
            );
            for hidden in [false, true] {
                let error = execute(&contexts, &forbidden, grant(), hidden)
                    .await
                    .unwrap_err();
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
                assert!(matches!(
                    error,
                    ProgramFault::Program(GraphMutationProgramError::Statement {
                        statement: 1,
                        source: GqlQueryError::Source(GraphMutationError::Source(
                            WriteTxnError::Authorization(Error::ScopeDenied)
                        )),
                    })
                ));
            }
            let program = program(UPDATE, &[(1, 11), (2, 22)]);
            let mut one_row = grant();
            one_row.limits.max_rows = 1;
            let error = execute(&contexts, &program, one_row, false)
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::LimitExceeded(LimitDimension::Rows))
            );
            assert!(matches!(
                error,
                ProgramFault::Program(GraphMutationProgramError::Statement { statement: 1, .. })
            ));

            let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
            let issuer = authority();
            let mut no_rows = grant();
            no_rows.limits.max_rows = 0;
            let token = issuer.issue_at(&no_rows, NOW).unwrap();
            let mut db = fixture(&commit, true).await;
            let (stats, _) = db
                .execute_graph_write_program_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.mutation_effects, 2);
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn overlapping_targets_refuse_rhs_feedback_and_keep_sequential_image_checks() {
        lab(0xa611_0104, |contexts| async move {
            // Eager selection does not freeze current property reads: the
            // second increment must observe q=2 and produce 5. Feedback is
            // refused until that RHS contract can accompany frozen bindings.
            let feedback = GraphUnwindWriteText::parse(
                "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.q=n.q+row.q,n.p=row.p",
            )
            .unwrap();
            assert!(matches!(
                feedback.bind(&arguments(&[(1, 2), (1, 3)]), R, symbols),
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ));
            let live = GraphUnwindWriteText::parse(
                "UNWIND $rows AS row MATCH (n {p:row.p}) SET n.q=n.q+row.q",
            )
            .unwrap()
            .bind(&arguments(&[(1, 2), (1, 3)]), R, symbols)
            .unwrap();
            assert!(!live.program().freezes_vertex_update_input());
            let (receipt, graph) = execute(&contexts, live.program(), grant(), true)
                .await
                .unwrap();
            assert_eq!(receipt.steps().len(), 2);
            for step in receipt.steps() {
                assert_eq!(step.mutation_targets(), Some([VId(1)].as_slice()));
            }
            assert_eq!(
                graph
                    .iter()
                    .find(|vertex| vertex.vid == VId(1))
                    .unwrap()
                    .props[1]
                    .1,
                CanonicalScalar::Int(5).into()
            );
            for (keyed, text) in [
                (true, UPDATE),
                (
                    false,
                    "UNWIND $rows AS row MATCH (n) \
                         WHERE n.p >= row.p AND n.p <= row.p SET n.q=row.q",
                ),
            ] {
                let constants = program(text, &[(1, 2), (1, 3)]);
                let (actual, actual_graph) = execute(&contexts, &constants, grant(), false)
                    .await
                    .unwrap();
                let (expected, expected_graph) =
                    execute(&contexts, &sequential(&constants), grant(), false)
                        .await
                        .unwrap();
                assert_eq!(actual_graph, expected_graph);
                if keyed {
                    assert_keyed_selection_receipts(&actual, &expected, 2);
                } else {
                    assert_eq!(actual, expected);
                }
            }
        });
    }
}

mod frozen_edge_updates {
    use super::*;
    use fgdb_gql::PreparedGraphWriteProgram;
    use fgdb_types::{CommitCx, EId};
    use fgdb_warden::LimitDimension;

    type ProgramFault =
        fgdb_gql::GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

    fn program(text: &str, values: &[(i64, i64)]) -> PreparedGraphWriteProgram {
        let bound = GraphUnwindWriteText::parse(text)
            .unwrap()
            .bind(&arguments(values), R, symbols)
            .unwrap();
        assert!(bound.program().freezes_edge_update_input());
        assert!(!bound.program().freezes_vertex_update_input());
        bound.into_program()
    }

    async fn fixture(commit: &CommitCx, hidden: bool) -> Database<MemVfs> {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![L], vec![]);
        batch.create_vertex(VId(2), vec![L], vec![]);
        for id in 1..=32 {
            let mut props = vec![(P, CanonicalScalar::Int(id)), (Q, CanonicalScalar::Int(0))];
            if hidden {
                props.push((SECRET, CanonicalScalar::Int(777)));
            }
            batch.add_edge(EId(id as u128), VId(1), VId(2), props);
        }
        if hidden {
            batch.create_vertex(VId(90), vec![HIDDEN], vec![]);
            batch.add_edge(
                EId(90),
                VId(1),
                VId(90),
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(555))],
            );
        }
        db.write(commit, batch).await.unwrap();
        if hidden {
            let mut other = WriteBatch::new(RelationId(2));
            other.add_edge(
                EId(91),
                VId(1),
                VId(2),
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(666))],
            );
            db.write(commit, other).await.unwrap();
        }
        db
    }

    async fn execute(
        contexts: &PurposeContexts,
        program: &PreparedGraphWriteProgram,
        scope: Grant,
        hidden: bool,
    ) -> Result<(GraphWriteProgramReceipt, Vec<fgdb::EdgeRecord>), ProgramFault> {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut db = fixture(&commit, hidden).await;
        let before = db.edges().unwrap();
        let frontier = db.frontier().unwrap();
        let result = db
            .execute_graph_write_program_returning_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                program,
                policy(),
                || NOW,
            )
            .await;
        assert_eq!(txn.outstanding_obligations(), 0);
        match result {
            Ok((receipt, completion)) => {
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: fgdb_types::CommitSeq(frontier.0 + 1)
                    }
                );
                let after = db.edges().unwrap();
                for original in &before {
                    let actual = after
                        .iter()
                        .find(|row| row.entry.eid == original.entry.eid)
                        .unwrap();
                    assert_eq!(actual.entry.src, original.entry.src);
                    assert_eq!(actual.entry.dst, original.entry.dst);
                    assert_eq!(actual.entry.relation, original.entry.relation);
                    if original.entry.eid.0 >= 90 {
                        assert_eq!(actual, original, "hidden edges stay unchanged");
                    }
                    assert_eq!(
                        actual.props.iter().find(|(key, _)| *key == SECRET),
                        original.props.iter().find(|(key, _)| *key == SECRET),
                    );
                }
                Ok((receipt, after))
            }
            Err(error) => {
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(db.edges().unwrap(), before);
                Err(error)
            }
        }
    }

    fn auth_error(error: &ProgramFault) -> Option<Error> {
        let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(error);
        while let Some(error) = cause {
            if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>()
            {
                return Some(*error);
            }
            cause = error.source();
        }
        None
    }

    async fn minimum_work(
        contexts: &PurposeContexts,
        program: &PreparedGraphWriteProgram,
        hidden: bool,
    ) -> u64 {
        let mut low = 0;
        let mut high = grant().limits.max_work;
        execute(contexts, program, grant(), hidden).await.unwrap();
        while low < high {
            let middle = low + (high - low) / 2;
            let mut scope = grant();
            scope.limits.max_work = middle;
            match execute(contexts, program, scope, hidden).await {
                Ok(_) => high = middle,
                Err(error) => {
                    assert_eq!(
                        auth_error(&error),
                        Some(Error::LimitExceeded(LimitDimension::Work)),
                        "{error:?}"
                    );
                    low = middle + 1;
                }
            }
        }
        low
    }

    #[test]
    fn edge_swaps_and_repeated_targets_freeze_inputs_without_observing_hidden_edges() {
        lab(0xa611_0201, |contexts| async move {
            let feedback = GraphUnwindWriteText::parse(
                "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p \
                 SET e.p=row.p, e.q=e.q+row.q",
            )
            .unwrap();
            assert!(matches!(
                feedback.bind(&arguments(&[(1, 2), (1, 3)]), R, symbols),
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ));
            let live = GraphUnwindWriteText::parse(
                "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p SET e.q=e.q+row.q",
            )
            .unwrap()
            .bind(&arguments(&[(1, 2), (1, 3)]), R, symbols)
            .unwrap();
            assert!(!live.program().freezes_edge_update_input());
            for hidden in [false, true] {
                let (_, graph) = execute(&contexts, live.program(), grant(), hidden)
                    .await
                    .unwrap();
                let edge = graph.iter().find(|row| row.entry.eid == EId(1)).unwrap();
                assert_eq!(edge.props[1], (Q, CanonicalScalar::Int(5).into()));
            }
            for values in [[(1, 2), (2, 1)], [(1, 2), (1, 3)]] {
                let program = program(
                    "UNWIND $rows AS row MATCH (a)-[e]->(b) \
                     WHERE e.p=row.p AND e.secret IS NULL SET e.p=row.q, e.q=row.q",
                    &values,
                );
                let mut visible_receipt = None;
                for hidden in [false, true] {
                    let (receipt, graph) =
                        execute(&contexts, &program, grant(), hidden).await.unwrap();
                    assert_eq!(receipt.stats().completed_statements, 2);
                    assert_eq!(receipt.stats().target_edge_visits, 2);
                    assert_eq!(receipt.stats().target_vertex_visits, 0);
                    for id in 1..=32 {
                        let mut expected = (id, 0);
                        for (p, q) in values {
                            if p == id {
                                expected = (q, q);
                            }
                        }
                        let edge = graph
                            .iter()
                            .find(|row| row.entry.eid == EId(id as u128))
                            .unwrap();
                        assert_eq!(edge.props[0], (P, CanonicalScalar::Int(expected.0).into()));
                        assert_eq!(edge.props[1], (Q, CanonicalScalar::Int(expected.1).into()));
                    }
                    if let Some(expected) = &visible_receipt {
                        assert_eq!(
                            &receipt, expected,
                            "hidden data cannot change native statistics"
                        );
                    } else {
                        visible_receipt = Some(receipt);
                    }
                }
            }
            // A denied original field must not disappear in normalization,
            // even when the assignment would preserve its stored value.
            let forbidden = program(
                "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p \
                 OPTIONAL MATCH (a)-[f:R]->(b) WHERE f.p=row.q \
                 SET e.p=row.q, f.secret=777",
                &[(1, 99), (2, 3)],
            );
            let error = execute(&contexts, &forbidden, grant(), true)
                .await
                .unwrap_err();
            assert_eq!(auth_error(&error), Some(Error::ScopeDenied));
            assert!(matches!(
                error,
                fgdb_gql::GraphWriteProgramError::Program(
                    fgdb_gql::GraphMutationProgramError::Statement { statement: 1, .. }
                )
            ));
            let valid = program(
                "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p SET e.p=row.q",
                &[(1, 2), (2, 1)],
            );
            let mut exhausted = grant();
            exhausted.limits.max_rows = 1;
            let error = execute(&contexts, &valid, exhausted, true)
                .await
                .unwrap_err();
            assert_eq!(
                auth_error(&error),
                Some(Error::LimitExceeded(LimitDimension::Rows))
            );
        });
    }

    #[test]
    fn disjoint_edge_updates_match_live_sequential_execution_with_linear_signed_work() {
        lab(0xa611_0202, |contexts| async move {
            let mut measured = Vec::new();
            for count in [8, 16, 32] {
                let values: Vec<_> = (1..=count).map(|id| (id, id + 100)).collect();
                let optimized = program(
                    "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p SET e.q=row.q",
                    &values,
                );
                let sequential =
                    PreparedGraphWriteProgram::prepare(optimized.statements().to_vec()).unwrap();
                assert!(!sequential.freezes_edge_update_input());
                let (actual, graph) = execute(&contexts, &optimized, grant(), true).await.unwrap();
                let (expected, expected_graph) = execute(&contexts, &sequential, grant(), true)
                    .await
                    .unwrap();
                assert_eq!(graph, expected_graph);
                assert_eq!(actual.steps(), expected.steps());
                assert_eq!(
                    actual.stats().mutation_effects,
                    expected.stats().mutation_effects
                );
                assert_eq!(
                    actual.stats().target_edge_visits,
                    expected.stats().target_edge_visits
                );
                let work = minimum_work(&contexts, &optimized, false).await;
                assert_eq!(work, minimum_work(&contexts, &optimized, true).await);
                assert!(work < minimum_work(&contexts, &sequential, false).await);
                measured.push(work);
            }
            // Source size is fixed at 32 visible edges. A doubled number of
            // statements may double real work, but may not replay a quadratic
            // growing preparation prefix. The incumbent runs above in this
            // invocation; no elapsed-time or throughput claim is made.
            for pair in measured.windows(2) {
                assert!(pair[1] <= pair[0] * 2, "{measured:?}");
            }
        });
    }
}

mod frozen_mixed_updates {
    use super::*;
    use fgdb_gql::{GraphMutationProgramError, PreparedGraphWriteProgram};
    use fgdb_types::EId;
    use fgdb_warden::LimitDimension;

    type ProgramFault =
        fgdb_gql::GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

    fn program(text: &str, values: &[(i64, i64)]) -> PreparedGraphWriteProgram {
        let bound = GraphUnwindWriteText::parse(text)
            .unwrap()
            .bind(&arguments(values), R, symbols)
            .unwrap();
        assert!(bound.program().freezes_mixed_update_input());
        bound.into_program()
    }

    #[allow(clippy::type_complexity)]
    async fn execute(
        contexts: &PurposeContexts,
        program: &PreparedGraphWriteProgram,
        scope: Grant,
        hidden: bool,
    ) -> Result<
        (
            GraphWriteProgramReceipt,
            Vec<fgdb::VertexRow>,
            Vec<fgdb::EdgeRecord>,
        ),
        ProgramFault,
    > {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for id in 1..=3 {
            let mut props = vec![(P, CanonicalScalar::Int(id)), (Q, CanonicalScalar::Int(0))];
            if hidden {
                props.push((SECRET, CanonicalScalar::Int(777)));
            }
            initial.create_vertex(VId(id as u128), vec![L], props);
        }
        for id in 1..=3 {
            let mut props = vec![(P, CanonicalScalar::Int(id)), (Q, CanonicalScalar::Int(0))];
            if hidden {
                props.push((SECRET, CanonicalScalar::Int(777)));
            }
            initial.add_edge(
                EId(id as u128),
                VId(id as u128),
                VId((id % 3 + 1) as u128),
                props,
            );
        }
        if hidden {
            initial.create_vertex(VId(90), vec![HIDDEN], vec![(P, CanonicalScalar::Int(1))]);
            initial.add_edge(EId(90), VId(1), VId(90), vec![(P, CanonicalScalar::Int(1))]);
        }
        db.write(&commit, initial).await.unwrap();
        if hidden {
            let mut other = WriteBatch::new(RelationId(2));
            other.add_edge(EId(91), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
            db.write(&commit, other).await.unwrap();
        }
        let before_vertices = db.vertices().unwrap();
        let before_edges = db.edges().unwrap();
        let frontier = db.frontier().unwrap();
        let result = db
            .execute_graph_write_program_returning_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                program,
                policy(),
                || NOW,
            )
            .await;
        assert_eq!(txn.outstanding_obligations(), 0);
        match result {
            Ok((receipt, completion)) => {
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: fgdb_types::CommitSeq(frontier.0 + 1),
                    }
                );
                let vertices = db.vertices().unwrap();
                let edges = db.edges().unwrap();
                for original in &before_vertices {
                    let actual = vertices.iter().find(|row| row.vid == original.vid).unwrap();
                    assert_eq!(actual.labels, original.labels);
                    assert_eq!(
                        actual.props.iter().find(|(key, _)| *key == SECRET),
                        original.props.iter().find(|(key, _)| *key == SECRET)
                    );
                    if original.vid == VId(90) {
                        assert_eq!(actual, original);
                    }
                }
                for original in &before_edges {
                    let actual = edges
                        .iter()
                        .find(|row| row.entry.eid == original.entry.eid)
                        .unwrap();
                    assert_eq!(
                        (
                            actual.entry.eid,
                            actual.entry.src,
                            actual.entry.dst,
                            actual.entry.relation
                        ),
                        (
                            original.entry.eid,
                            original.entry.src,
                            original.entry.dst,
                            original.entry.relation
                        )
                    );
                    assert_eq!(
                        actual.props.iter().find(|(key, _)| *key == SECRET),
                        original.props.iter().find(|(key, _)| *key == SECRET)
                    );
                    if original.entry.eid.0 >= 90 {
                        assert_eq!(actual, original);
                    }
                }
                Ok((receipt, vertices, edges))
            }
            Err(error) => {
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(db.vertices().unwrap(), before_vertices);
                assert_eq!(db.edges().unwrap(), before_edges);
                Err(error)
            }
        }
    }

    fn auth_error(error: &ProgramFault) -> Option<Error> {
        let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(error);
        while let Some(error) = cause {
            if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>()
            {
                return Some(*error);
            }
            cause = error.source();
        }
        None
    }

    #[test]
    fn mixed_rekeys_preserve_visible_graphs_receipts_and_hidden_data_independence() {
        lab(0xa611_0301, |contexts| async move {
            for values in [[(1, 2), (2, 1)], [(1, 2), (2, 3)], [(1, 2), (1, 3)]] {
                let program = program(
                    "UNWIND $rows AS row MATCH (a:Visible {p:row.p})-[e:R {p:row.p}]->(b) \
                     SET a.p=row.q,e.p=row.q,e.q=row.q",
                    &values,
                );
                let mut visible_receipt = None;
                for hidden in [false, true] {
                    let (receipt, vertices, edges) =
                        execute(&contexts, &program, grant(), hidden).await.unwrap();
                    assert_eq!(receipt.stats().completed_statements, 2);
                    assert_eq!(receipt.stats().target_vertex_visits, 2);
                    assert_eq!(receipt.stats().target_edge_visits, 2);
                    assert_eq!(receipt.stats().mutation_effects, 6);
                    for (step, (p, _)) in receipt.steps().iter().zip(values) {
                        assert_eq!(step.mutation_targets(), Some([VId(p as u128)].as_slice()));
                        assert_eq!(step.mutation_edges(), Some([EId(p as u128)].as_slice()));
                    }
                    for id in 1..=3 {
                        let mut expected = (id, 0);
                        for (p, q) in values {
                            if p == id {
                                expected = (q, q);
                            }
                        }
                        let vertex = vertices
                            .iter()
                            .find(|row| row.vid == VId(id as u128))
                            .unwrap();
                        assert_eq!(
                            vertex.props[0],
                            (P, CanonicalScalar::Int(expected.0).into())
                        );
                        assert_eq!(vertex.props[1], (Q, CanonicalScalar::Int(0).into()));
                        let edge = edges
                            .iter()
                            .find(|row| row.entry.eid == EId(id as u128))
                            .unwrap();
                        assert_eq!(edge.props[0], (P, CanonicalScalar::Int(expected.0).into()));
                        assert_eq!(edge.props[1], (Q, CanonicalScalar::Int(expected.1).into()));
                    }
                    if let Some(expected) = &visible_receipt {
                        assert_eq!(
                            &receipt, expected,
                            "hidden state cannot affect native work or receipts"
                        );
                    } else {
                        visible_receipt = Some(receipt);
                    }
                }
            }
        });
    }

    #[test]
    fn mixed_staging_keeps_original_field_checks_endpoint_scope_and_cumulative_rows() {
        lab(0xa611_0302, |contexts| async move {
            let forbidden = program(
                "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p \
                 OPTIONAL MATCH (b)-[f:R]->(c) WHERE f.p=row.q \
                 SET a.q=row.q,e.p=row.q,f.secret=777",
                &[(1, 99), (2, 3)],
            );
            let exits_scope = program(
                "UNWIND $rows AS row MATCH (a:Visible)-[e:R]->(b) WHERE e.p=row.p \
                 SET e.p=row.q REMOVE a:Visible",
                &[(1, 2), (2, 1)],
            );
            let valid = program(
                "UNWIND $rows AS row MATCH (a)-[e:R]->(b) WHERE e.p=row.p SET a.q=row.q,e.p=row.q",
                &[(1, 2), (2, 1)],
            );
            for hidden in [false, true] {
                for (program, expected_row) in [(&forbidden, 1), (&exits_scope, 0)] {
                    let error = execute(&contexts, program, grant(), hidden)
                        .await
                        .unwrap_err();
                    assert_eq!(auth_error(&error), Some(Error::ScopeDenied));
                    assert!(matches!(error,
                        ProgramFault::Program(GraphMutationProgramError::Statement { statement, .. })
                            if statement == expected_row
                    ));
                }
                let mut exact = grant();
                exact.limits.max_rows = 4; // One vertex and one edge in each of two receipts.
                execute(&contexts, &valid, exact.clone(), hidden)
                    .await
                    .unwrap();
                exact.limits.max_rows -= 1;
                let error = execute(&contexts, &valid, exact, hidden).await.unwrap_err();
                assert_eq!(
                    auth_error(&error),
                    Some(Error::LimitExceeded(LimitDimension::Rows))
                );
                assert!(matches!(
                    error,
                    ProgramFault::Program(GraphMutationProgramError::Statement {
                        statement: 1,
                        ..
                    })
                ));
            }
        });
    }
}
