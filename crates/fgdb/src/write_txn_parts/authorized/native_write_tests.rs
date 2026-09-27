//! Public text ingress over real Warden permits, native programs and Chronicle.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{GqlParameterValue, GqlQueryPolicy, GqlScalarParameter};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x91; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x90; 32], NS, [0x92; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9991), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
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
fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}

#[test]
fn native_text_has_one_commit_dependent_receipts_and_reopen() {
    lab(0xaa11, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let params = GqlParameters::new()
            .with_int64("left", 10)
            .unwrap()
            .with_int64("right", 20)
            .unwrap();
        let mut calls = BTreeMap::new();
        let result = db
            .query_write_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                "CREATE (a:Visible {p:$left}), (b:Visible {p:$right}), (a)-[:R {p:1}]->(b); \
             MATCH (a:Visible)-[e:R]->(b:Visible) SET a.q=b.p+1, e.p=9; \
             MATCH (a:Visible)-[e:R]->(b:Visible) DELETE e",
                &params,
                |kind, name| {
                    *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
                    symbols(kind, name)
                },
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = result
        else {
            panic!("write receipt")
        };
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (2, 1)
        );
        assert_eq!(
            receipt.steps()[0].created_vertices(),
            Some(&[VId(1), VId(2)][..])
        );
        assert_eq!(receipt.steps()[1].mutation_targets(), Some(&[VId(1)][..]));
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(
            completion,
            Some(fgdb_types::EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq })
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(10)), (Q, CanonicalScalar::Int(21))]
        );
        assert!(db.edge(EId(1)).unwrap().is_none());
        assert!(!calls.is_empty());
        assert!(
            calls.values().all(|count| *count == 1),
            "one frozen catalog resolution"
        );
        let state = (seq, db.vertices().unwrap(), db.edges().unwrap());
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
            state
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn authentication_and_write_rights_precede_syntax_arguments_and_catalog() {
    lab(0xaa12, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut scope = grant();
            if mode == 0 {
                scope.rights = Rights::Read;
            }
            let token = authority.issue_at(&scope, NOW).unwrap();
            let other = Authority::new(
                AuthKey::from_seed(9992),
                DatabaseSecurityNamespaceId([0x93; 32]),
                "graph",
                SchemaEpoch(1),
                1,
            )
            .unwrap();
            let host = if mode == 2 { &other } else { &authority };
            let mut calls = 0;
            let error = db
                .query_write_authorized(
                    &txn,
                    &query,
                    &commit,
                    host,
                    &token,
                    if mode == 1 { "other" } else { "main" },
                    "CREATE (n:Visible {p:$missing}); unsupported tail",
                    &GqlParameters::new(),
                    |kind, name| {
                        calls += 1;
                        symbols(kind, name)
                    },
                    R,
                    policy(),
                    || if mode == 3 { 20_000 } else { NOW },
                )
                .await
                .unwrap_err();
            assert!(authorization(&error).is_some(), "mode {mode}: {error:?}");
            assert_eq!(calls, 0);
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn definition_admission_precedes_catalog_and_cannot_reset_before_execution() {
    lab(0xaa13, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let text = format!("{}CREATE (n:Visible)", " ".repeat(4096));
        let params = GqlParameters::new();
        let prepared = PreparedGraphWriteScript::prepare(&text, R, symbols).unwrap();
        for spare in [0, 1] {
            let mut scope = grant();
            scope.limits.max_work = text.len() as u64 - 1 + spare;
            let token = authority.issue_at(&scope, NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut calls = 0;
            let error = db
                .query_write_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &text,
                    &params,
                    |kind, name| {
                        calls += 1;
                        symbols(kind, name)
                    },
                    R,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(authorization(&error).is_some(), "{error:?}");
            assert_eq!(
                calls, 0,
                "the pre-lookup checkpoint shares the ingress bill"
            );
            assert_eq!(db.frontier().unwrap(), before);
            // With caller-owned preparation there is no byte bill. The exact
            // same token can execute this small program, so text rejection is
            // not merely an impossibly small execution quota.
            db.execute_graph_write_script_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared,
                &params,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn expiry_in_a_catalog_callback_is_not_misreported_as_unknown_symbol() {
    lab(0xaa14, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for found in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let time = AtomicU64::new(NOW);
            let mut calls = 0;
            let error = db
                .query_write_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    "CREATE (n:Visible {p:7, q:9})",
                    &GqlParameters::new(),
                    |kind, name| {
                        calls += 1;
                        time.store(20_000, Ordering::SeqCst);
                        if found { symbols(kind, name) } else { None }
                    },
                    R,
                    policy(),
                    || time.load(Ordering::SeqCst),
                )
                .await
                .unwrap_err();
            assert!(authorization(&error).is_some(), "{error:?}");
            assert!(!matches!(error, Fault::Binding(_)));
            assert_eq!(calls, 1);
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(VId(1)),
                "preparation cannot reserve a graph identity"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn malformed_or_late_unbound_text_never_stages_its_valid_prefix() {
    lab(0xaa15, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for text in [
            "CREATE (n:Visible {p:1}); MATCH (n:Visible) SET n.q=$missing",
            "CREATE (n:Visible {p:1}); RETURN n",
            "CREATE (n:Visible {p:1}); MATCH (n:Visible) SET n.q=",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .query_write_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, Fault::Binding(_)), "{text}: {error:?}");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(VId(1))
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn string_parameters_remain_data_and_write_only_creation_remains_usable() {
    lab(0xaa16, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut scope = grant();
        scope.rights = Rights::Write;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let value = CanonicalScalar::ucs_basic_text("'); MATCH (n) DETACH DELETE n; -- é").unwrap();
        let mut params = GqlParameters::new();
        params
            .insert(
                "payload",
                GqlParameterValue::Scalar(GqlScalarParameter::new(value.clone()).unwrap()),
            )
            .unwrap();
        let result = db
            .query_write_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                "CREATE (n:Visible {p:$payload})",
                &params,
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            QueryResult::Write {
                completion: Some(_),
                ..
            }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, vec![(P, value)]);
        let before = db.frontier().unwrap();
        let error = db
            .query_write_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                "CREATE (m:Visible); MATCH (n:Visible) SET n.q=1",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::PermissionDenied));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn masked_selection_and_forbidden_noop_tails_use_authorized_native_staging() {
    lab(0xaa17, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut scope = grant();
        scope.labels = Scope::only([L]);
        scope.properties = Scope::only([P, Q]);
        let token = authority.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = crate::WriteBatch::new(R);
        seed.create_vertex(
            VId(10),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(1)),
                (SECRET, CanonicalScalar::Int(7)),
            ],
        );
        seed.create_vertex(VId(20), vec![HIDDEN], vec![(P, CanonicalScalar::Int(2))]);
        db.write(&commit, seed).await.unwrap();
        let result = db
            .query_write_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                "MATCH (n) SET n.q=42",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        let QueryResult::Write { receipt, .. } = result else {
            panic!("write receipt")
        };
        assert_eq!(receipt.steps()[0].mutation_targets(), Some(&[VId(10)][..]));
        assert_eq!(
            db.vertex(VId(20)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2))]
        );
        assert!(
            db.vertex(VId(10))
                .unwrap()
                .unwrap()
                .props
                .contains(&(SECRET, CanonicalScalar::Int(7)))
        );
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let error = db
            .query_write_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                "CREATE (m:Visible {p:9}); MATCH (n:Visible) WHERE n.p=1 SET n.secret=7",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn receipt_refusal_and_unpolled_futures_publish_nothing() {
    lab(0xaa18, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut scope = grant();
        scope.limits.max_rows = 1;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let text = "CREATE (n:Visible {p:1}); MATCH (n:Visible) SET n.q=2";
        let params = GqlParameters::new();
        let mut calls = 0;
        drop(db.query_write_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            text,
            &params,
            |kind, name| {
                calls += 1;
                symbols(kind, name)
            },
            R,
            policy(),
            || NOW,
        ));
        assert_eq!(calls, 0);
        assert_eq!(db.frontier().unwrap(), before);
        let error = db
            .query_write_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                text,
                &params,
                symbols,
                R,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(authorization(&error).is_some(), "{error:?}");
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
