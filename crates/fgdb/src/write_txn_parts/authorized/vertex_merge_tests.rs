//! Capability-scoped get-or-create uses the real collector and Chronicle path.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GlaLimitDimension, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    PreparedGraphVertexMerge, PreparedGraphVertexMergeText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x64; 32]);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x63; 32], NS, [0x65; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9981), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits { max_nodes: 10_000, max_work: 1_000_000, max_rows: 1 },
        expires_at_ms: EXPIRES,
    }
}
fn policy() -> GraphVertexMergePolicy {
    GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000))
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn merge(text: &str) -> PreparedGraphVertexMerge {
    PreparedGraphVertexMergeText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn authorization(error: MergeFault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphVertexMergeError::Source(WriteTxnError::Authorization(error))) => error,
        other => panic!("expected a typed authorization refusal, got {other:?}"),
    }
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
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, labels: Vec<LabelId>, id: u128) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(id), labels, vec![(P, CanonicalScalar::Int(7))]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn scoped_merge_creates_once_matches_without_a_commit_and_reopens() {
    lab(0xa981, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit, vec![HIDDEN], 100).await;
        let hidden = db.vertex(VId(100)).unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let definition = merge("MERGE (n:Visible {p:7})");
        let (stats, first, completion) = db.execute_graph_vertex_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &definition, policy(), || NOW,
        ).await.unwrap();
        assert!(first.created());
        assert!(first.vertex().0 > 100);
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(stats.match_selection.result_rows, 0);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, basis.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        assert!(pinned.vertex(first.vertex()).unwrap().is_none());
        assert_eq!(db.vertex(VId(100)).unwrap(), hidden);
        let (stats, second, completion) = db.execute_graph_vertex_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &definition,
            policy().with_creation_limit(0), || NOW,
        ).await.unwrap();
        assert!(matches!(second, GraphVertexMergeOutcome::Matched(_)));
        assert_eq!(second.vertex(), first.vertex());
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(completion, EmbeddedTxnCompletion::ReadClosed {
            snapshot_seq: seq, validated_through: seq,
        });
        assert_eq!(db.frontier().unwrap(), seq);
        // The match arm neither invokes allocation nor publishes a no-op.
        let next = db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap();
        assert_eq!(next, ElementId::Vertex(VId(first.vertex().0 + 1)));
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        let (_, reopened, _) = db.execute_graph_vertex_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &definition, policy(), || NOW,
        ).await.unwrap();
        assert_eq!(reopened.vertex(), first.vertex());
        assert!(!reopened.created());
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertex(VId(100)).unwrap(), hidden);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn ambiguity_counts_only_visible_identities_and_never_allocates_or_publishes() {
    lab(0xa982, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let definition = merge("MERGE (n:Visible {p:7})");
        for visible in [1_u128, 2] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            for id in 1..=visible { seed(&mut db, &commit, vec![L], id).await; }
            for id in 100..104 { seed(&mut db, &commit, vec![HIDDEN], id).await; }
            let basis = db.frontier().unwrap();
            let rows = db.vertices().unwrap();
            let result = db.execute_graph_vertex_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &definition, policy(), || NOW,
            ).await;
            if visible == 1 {
                assert_eq!(result.unwrap().1.vertex(), VId(1));
            } else {
                assert!(matches!(result, Err(GqlQueryError::Source(
                    GraphVertexMergeError::AmbiguousMatches { observed: 2 }
                ))));
            }
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(db.vertices().unwrap(), rows);
            assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(),
                ElementId::Vertex(VId(104)));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn readwrite_and_one_receipt_row_are_required_before_graph_access_or_allocation() {
    lab(0xa983, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let definition = merge("MERGE (n:Visible {p:7})");
        for rights in [Rights::Read, Rights::Write, Rights::ReadWrite] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut request = grant();
            request.rights = rights;
            let token = authority.issue_at(&request, NOW).unwrap();
            let token = if rights == Rights::ReadWrite {
                token.attenuate(Restriction::MaxRows(0)).unwrap()
            } else { token };
            let error = db.execute_graph_vertex_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &definition, policy(), || NOW,
            ).await.unwrap_err();
            let expected = if rights == Rights::ReadWrite {
                Error::LimitExceeded(LimitDimension::Rows)
            } else { Error::PermissionDenied };
            assert_eq!(authorization(error), expected);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(),
                ElementId::Vertex(VId(1)));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn creation_limits_and_forbidden_fields_refuse_without_partial_effects() {
    lab(0xa984, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for text in ["MERGE (n:Hidden {p:7})", "MERGE (n:Visible {secret:7})"] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let error = db.execute_graph_vertex_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &merge(text), policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let error = db.execute_graph_vertex_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &merge("MERGE (n:Visible {p:7})"),
            policy().with_creation_limit(0), || NOW,
        ).await.unwrap_err();
        assert!(matches!(error, GqlQueryError::Source(GraphVertexMergeError::Creation(
            GraphInsertError::Limit { dimension: GraphInsertLimitDimension::Vertices, limit: 0, .. }
        ))));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(),
            ElementId::Vertex(VId(1)));
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn match_and_creation_share_the_original_native_work_and_scratch_limits() {
    lab(0xa985, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let definition = merge("MERGE (n:Visible {p:7})");
        let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
        let (stats, _, _) = baseline.execute_graph_vertex_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &definition, policy(), || NOW,
        ).await.unwrap();
        let work = stats.evaluator.work_units;
        let scratch = stats.evaluator.scratch_entries;
        assert!(work > 0 && scratch > 0);
        for dimension in [GlaLimitDimension::WorkUnits, GlaLimitDimension::ScratchEntries] {
            for below in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let basis = db.frontier().unwrap();
                let (max_work, max_scratch) = match dimension {
                    GlaLimitDimension::WorkUnits => (work - u64::from(below), scratch),
                    GlaLimitDimension::ScratchEntries => (work, scratch - u64::from(below)),
                };
                let bounded = GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, max_work, max_scratch));
                let result = db.execute_graph_vertex_merge_authorized(
                    &txn, &query, &commit, &authority, &token, "main", &definition, bounded, || NOW,
                ).await;
                if below {
                    match result {
                        Err(GqlQueryError::Evaluator(error)) => {
                            assert_eq!(error.dimension, dimension);
                            assert_eq!(error.limit, if dimension == GlaLimitDimension::WorkUnits { max_work } else { max_scratch });
                        }
                        other => panic!("expected cumulative evaluator refusal, got {other:?}"),
                    }
                    assert_eq!(db.frontier().unwrap(), basis);
                    assert!(db.vertices().unwrap().is_empty());
                } else {
                    assert_eq!(result.unwrap().0.evaluator, stats.evaluator);
                    assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn expiry_at_every_live_check_refuses_before_publication_and_releases_the_pin() {
    lab(0xa986, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let definition = merge("MERGE (n:Visible {p:7})");
        for existing in [false, true] {
            let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
            if existing { seed(&mut baseline, &commit, vec![L], 1).await; }
            let count = Arc::new(AtomicU64::new(0));
            let clock_count = Arc::clone(&count);
            baseline.execute_graph_vertex_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &definition, policy(),
                move || { clock_count.fetch_add(1, Ordering::SeqCst); NOW },
            ).await.unwrap();
            let checks = count.load(Ordering::SeqCst);
            assert!(checks > 3);
            for expire_at in 1..=checks {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                if existing { seed(&mut db, &commit, vec![L], 1).await; }
                let basis = db.frontier().unwrap();
                let rows = db.vertices().unwrap();
                let mut calls = 0_u64;
                let result = db.execute_graph_vertex_merge_authorized(
                    &txn, &query, &commit, &authority, &token, "main", &definition, policy(),
                    move || { calls += 1; if calls >= expire_at { EXPIRES + 1 } else { NOW } },
                ).await;
                assert!(result.is_err(), "expiry check {expire_at}/{checks}, matched={existing}");
                assert_eq!(db.frontier().unwrap(), basis);
                assert_eq!(db.vertices().unwrap(), rows);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn native_merge_keeps_negative_match_conflict_witnesses_after_collector_extraction() {
    lab(0xa987, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let (stats, created) = transaction.execute_graph_vertex_merge_governed(
            &mut db, &query, &merge("MERGE (n:Visible {p:7})"), policy(),
            |_| Ok::<_, WriteTxnError>(ElementId::Vertex(VId(1))),
        ).unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(created.vertex(), VId(1));
        seed(&mut db, &commit, vec![L], 2).await;
        let frontier = db.frontier().unwrap();
        assert!(matches!(transaction.commit(&mut db, &commit).await,
            Err(WriteTxnError::Write(crate::WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
