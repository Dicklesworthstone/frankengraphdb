//! Native UNWIND ingress shares real Warden permits and one Chronicle completion.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch, WriteTxnError};
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
        GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 100_000), 100, 100, 100,
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
    ]).unwrap()
}
fn arguments(values: &[(i64, i64)]) -> GqlParameters {
    GqlParameters::new().with_list("rows", values.iter()
        .map(|&(p, q)| row(p, CanonicalScalar::Int(q))).collect()).unwrap()
}
fn receipt(result: QueryResult) -> (GraphWriteProgramReceipt, EmbeddedTxnCompletion) {
    match result {
        QueryResult::Write { receipt, completion: Some(completion) } => (receipt, completion),
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
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let args = arguments(&[(1, 2), (2, 4), (1, 3)]);
        let original = args.canonical_bytes();
        let (result, completion) = receipt(db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", COUNTER, &args,
            symbols, R, policy(), || NOW,
        ).await.unwrap());
        assert_eq!(result.stats().completed_statements, 3);
        assert_eq!(result.stats().created_vertices, 2);
        let first = result.steps()[0].merged_vertex().unwrap().vertex();
        let second = result.steps()[1].merged_vertex().unwrap().vertex();
        assert_eq!(result.steps()[2].merged_vertex().unwrap().vertex(), first);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        assert!(pinned.vertex(first).unwrap().is_none());
        assert_eq!(args.canonical_bytes(), original);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.vertex(first).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(5))]);
        assert_eq!(db.vertex(second).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(4))]);
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
        let error = db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", text, &args,
            symbols, R, policy(), || NOW,
        ).await.unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert!(matches!(error, Fault::Program(_)), "definition admission precedes execution");
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let (result, _) = receipt(db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", text, &args,
            symbols, R, policy(), || NOW,
        ).await.unwrap());
        assert_eq!(result.stats().created_edges, 1);
        assert_eq!(result.stats().completed_statements, 2);
        let edge = result.steps()[0].merged_edge().unwrap().edge().unwrap();
        assert_eq!(result.steps()[1].merged_edge().unwrap().edge(), Some(edge));
        assert_eq!(db.edge(edge).unwrap().unwrap().props, vec![(Q, CanonicalScalar::Int(5))]);
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
            scope.rights = match mode { 0 => Rights::Read, 1 => Rights::Write, _ => Rights::ReadWrite };
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let calls = AtomicUsize::new(0);
            let error = db.query_write_authorized(
                &txcx, &query, &commit, &issuer, &token,
                if mode == 2 { "other" } else { "main" }, COUNTER, &GqlParameters::new(),
                |kind, name| { calls.fetch_add(1, Ordering::Relaxed); symbols(kind, name) },
                R, policy(), || if mode == 3 { EXPIRES + 1 } else { NOW },
            ).await.unwrap_err();
            assert!(authorization(&error).is_some(), "mode {mode}: {error:?}");
            if mode <= 1 { assert_eq!(authorization(&error), Some(Error::PermissionDenied)); }
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            if mode == 1 {
                // A write-only grant is usable, just not for a reading UNWIND.
                db.query_write_authorized(
                    &txcx, &query, &commit, &issuer, &token, "main", "CREATE (:Visible {p:1})",
                    &GqlParameters::new(), symbols, R, policy(), || NOW,
                ).await.unwrap();
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
        let args = GqlParameters::new().with_list("rows", vec![
            row(1, CanonicalScalar::Int(2)), row(2, CanonicalScalar::Bool(true)),
        ]).unwrap();
        let error = db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", COUNTER, &args,
            |_, _| panic!("late type refusal must precede catalog access"), R, policy(), || NOW,
        ).await.unwrap_err();
        assert!(matches!(error, Fault::UnwindBinding(GraphUnwindWriteError::Row {
            row: 1, kind: GraphUnwindRowError::IncompatibleFieldTypes, ..
        })));
        let error = db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", COUNTER,
            &arguments(&vec![(1, 1); 65]), |_, _| panic!("row cap precedes catalog"),
            R, policy(), || NOW,
        ).await.unwrap_err();
        assert!(matches!(error, Fault::UnwindBinding(GraphUnwindWriteError::TooManyRows {
            limit: 64, observed: 65,
        })));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
            .unwrap(), ElementId::Vertex(VId(1)), "binding must not reserve graph IDs");
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
                1 => { allowance.mutations.max_effects = 2; arguments(&[(1, 2), (1, 3)]) }
                2 => { allowance.max_created_vertices = 1; arguments(&[(1, 2), (2, 3)]) }
                _ => { scope.limits.max_rows = 1; arguments(&[(1, 2), (1, 3)]) }
            };
            let token = issuer.issue_at(&scope, NOW).unwrap();
            let before = db.frontier().unwrap();
            let error = db.query_write_authorized(
                &txcx, &query, &commit, &issuer, &token, "main", COUNTER, &args,
                symbols, R, allowance, || NOW,
            ).await.unwrap_err();
            if mode == 3 { assert!(authorization(&error).is_some(), "{error:?}"); }
            let Fault::BatchProgram { location: Some(location), .. } = error else {
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
        let error = db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", COUNTER, &arguments(&[(1, 2)]),
            |_, _| {
                calls.fetch_add(1, Ordering::Relaxed);
                now.store(EXPIRES + 1, Ordering::Relaxed);
                None
            }, R, policy(), || now.load(Ordering::Relaxed),
        ).await.unwrap_err();
        assert!(authorization(&error).is_some(), "expiry cannot become a parser error: {error:?}");
        assert!(matches!(error, Fault::Program(_)), "no execution record is fabricated");
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
            seed.create_vertex(VId(1), vec![L], vec![
                (P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(10)), (SECRET, secret.clone()),
            ]);
            seed.create_vertex(VId(99), vec![HIDDEN], vec![(P, CanonicalScalar::Int(1))]);
            db.write(&commit, seed).await.unwrap();
            let hidden = db.vertex(VId(99)).unwrap();
            let clock_calls = AtomicUsize::new(0);
            let result = db.query_write_authorized(
                &txcx, &query, &commit, &issuer, &token, "main",
                "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) \
                 SET n.q=n.q+COALESCE(n.secret,0)+row.q",
                &arguments(&[(1, 2), (1, 3)]), symbols, R, policy(),
                || { clock_calls.fetch_add(1, Ordering::Relaxed); NOW },
            ).await.unwrap();
            let trace = (result, clock_calls.load(Ordering::Relaxed));
            if let Some(expected) = &first { assert_eq!(&trace, expected); } else { first = Some(trace); }
            let stored = db.vertex(VId(1)).unwrap().unwrap();
            assert_eq!(stored.props, vec![
                (P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(15)), (SECRET, secret),
            ]);
            assert_eq!(db.vertex(VId(99)).unwrap(), hidden);
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let error = db.query_write_authorized(
                &txcx, &query, &commit, &issuer, &token, "main",
                "UNWIND $rows AS row MERGE (n:Visible {p:row.p}) \
                 ON CREATE SET n.secret=row.q SET n.q=5",
                &arguments(&[(2, 3)]), symbols, R, policy(), || NOW,
            ).await.unwrap_err();
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
        let args = GqlParameters::new().with_list("rows", vec![
            row(1, payload.clone()), row(1, payload),
        ]).unwrap();
        let mut binding_work = 0;
        let bound = GraphUnwindWriteText::parse(text).unwrap()
            .bind_with_limit_controlled(&args, R, 64, symbols, |event| {
                if let GraphUnwindBindEvent::Work(units) = event { binding_work += units; }
                Ok::<_, ()>(())
            }).unwrap();
        let mut scope = grant();
        // Original-text admission, post-classification check, then exactly the
        // controlled binding bill. There is no spare unit for program admission.
        scope.limits.max_work = text.len() as u64 + 1 + binding_work;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let error = db.query_write_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", text, &args,
            symbols, R, policy(), || NOW,
        ).await.unwrap_err();
        assert!(authorization(&error).is_some(), "{error:?}");
        assert!(matches!(error, Fault::BatchProgram { location: None, .. }),
            "binding must finish and the SAME exhausted permit must refuse execution");
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        // The same grant can execute the already-bound program. This rules out
        // a quota too small for the write itself as the reason for refusal.
        let (receipt, _) = db.execute_graph_write_program_returning_authorized(
            &txcx, &query, &commit, &issuer, &token, "main", bound.program(), policy(), || NOW,
        ).await.unwrap();
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
