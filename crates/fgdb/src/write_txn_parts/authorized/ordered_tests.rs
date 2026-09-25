//! Public authorized composition over the real native transaction/Chronicle path.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{RelationId, SchemaEpoch};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Rights, Scope};

const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xb3; 32]);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb1; 32], NAMESPACE, [0xb2; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xb301), NAMESPACE, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S, T]),
        properties: Scope::only([P]),
        rights: Rights::Write,
        limits: QueryLimits {
            max_nodes: 100,
            max_work: 4096,
            max_rows: 0,
        },
        expires_at_ms: 10_000,
    }
}

fn under_lab<Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static)
where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(report.lab_test_passed(), "lab invariants/quiescence: {report:?}");
}

fn dependent_batches() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(R);
    first.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
    first.create_vertex(VId(2), vec![L], vec![]);
    first.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(10))]);
    let mut second = WriteBatch::new(S);
    second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2)));
    second.create_vertex(VId(3), vec![L], vec![]);
    second.add_edge(EId(20), VId(2), VId(3), vec![]);
    let mut third = WriteBatch::new(T);
    third.compare_and_set_vertex_property(
        VId(1),
        P,
        Some(CanonicalScalar::Int(2)),
        CanonicalScalar::Int(3),
        WriteMismatchPolicy::AbortWrite,
    );
    // Identity-addressed mutations must retain the actual R coordinate.
    third.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(11)));
    third.add_edge(EId(30), VId(3), VId(1), vec![]);
    let mut last = WriteBatch::new(R);
    last.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![(P, CanonicalScalar::Int(99))]);
    vec![first, second, third, last]
}

fn assert_graph<V: Vfs + Clone>(db: &Database<V>, seq: CommitSeq) {
    assert_eq!(db.frontier().unwrap(), seq);
    assert_eq!(
        db.vertex(VId(1)).unwrap().unwrap().props,
        vec![(P, CanonicalScalar::Int(3))]
    );
    for vid in [VId(1), VId(2), VId(3)] {
        assert_eq!(db.vertex(vid).unwrap().unwrap().labels, vec![L]);
    }
    assert_eq!(
        db.edge_at(EId(10), seq).unwrap().unwrap().props,
        vec![(P, CanonicalScalar::Int(11))]
    );
    assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(2)]);
    assert_eq!(db.neighbours(VId(2), S).unwrap(), vec![VId(3)]);
    assert_eq!(db.neighbours(VId(3), T).unwrap(), vec![VId(1)]);
    assert!(db.edge_at(EId(99), seq).unwrap().is_none());
}

#[test]
fn dependent_relations_authorize_original_intents_and_reopen_one_commit() {
    under_lab(0xb301, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let path = std::env::temp_dir().join(format!(
            "fgdb-authorized-ordered-reopen-{}",
            std::process::id()
        ));
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let initial = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let seq = db
            .write_ordered_authorized(
                &txn, &cx, &authority, &token, "main", dependent_batches(), || NOW,
            )
            .await
            .unwrap();
        assert_eq!(seq.0, initial.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_graph(&db, seq);
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_graph(&reopened, seq);
    });
}

#[test]
fn forbidden_noop_in_another_relation_discards_all_prefix_effects() {
    under_lab(0xb302, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(9), vec![L], vec![(SECRET, CanonicalScalar::Int(99))]);
        db.write(&cx, seed).await.unwrap();
        let initial = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut batches = dependent_batches();
        let mut denied_tail = WriteBatch::new(S);
        denied_tail.set_vertex_property(VId(9), SECRET, Some(CanonicalScalar::Int(99)));
        batches.push(denied_tail);
        assert!(matches!(
            db.write_ordered_authorized(&txn, &cx, &authority, &token, "main", batches, || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), initial);
        assert_eq!(txn.outstanding_obligations(), baseline);
        for vid in [VId(1), VId(2), VId(3)] {
            assert!(db.vertex(vid).unwrap().is_none());
        }
        for eid in [EId(10), EId(20), EId(30), EId(99)] {
            assert!(db.edge_at(eid, initial).unwrap().is_none());
        }
        assert_eq!(
            db.vertex(VId(9)).unwrap().unwrap().props,
            vec![(SECRET, CanonicalScalar::Int(99))]
        );
        // The failed workspace must not reserve identities or fence the handle.
        let seq = db
            .write_ordered_authorized(
                &txn, &cx, &authority, &token, "main", dependent_batches(), || NOW,
            )
            .await
            .unwrap();
        assert_eq!(seq.0, initial.0 + 1);
        assert_graph(&db, seq);
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}

#[test]
fn batch_coordinate_cannot_authorize_a_hidden_edge_relation() {
    under_lab(0xb303, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(T);
        seed.create_vertex(VId(1), vec![L], vec![]);
        seed.create_vertex(VId(2), vec![L], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
        db.write(&cx, seed).await.unwrap();
        let initial = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = authority();
        let mut scoped = grant();
        scoped.relations = Scope::only([R, S]);
        let token = authority.issue_at(&scoped, NOW).unwrap();
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(3), vec![L], vec![]);
        let mut second = WriteBatch::new(S);
        second.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(2)));
        assert!(matches!(
            db.write_ordered_authorized(
                &txn, &cx, &authority, &token, "main", vec![first, second], || NOW,
            )
            .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), initial);
        assert!(db.vertex(VId(3)).unwrap().is_none());
        assert_eq!(
            db.edge_at(EId(10), initial).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1))]
        );
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}

#[test]
fn node_allowance_is_shared_across_relation_boundaries() {
    under_lab(0xb304, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
        let initial = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = authority();
        let mut scoped = grant();
        // One creation pays initial admission and its actual after-image.
        // Each batch alone fits two nodes; together they need four.
        scoped.limits.max_nodes = 2;
        let token = authority.issue_at(&scoped, NOW).unwrap();
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![L], vec![]);
        let mut second = WriteBatch::new(S);
        second.create_vertex(VId(2), vec![L], vec![]);
        let batches = vec![first, second];
        assert!(matches!(
            db.write_ordered_authorized(
                &txn, &cx, &authority, &token, "main", batches.clone(), || NOW,
            )
            .await,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(LimitDimension::Nodes)))
        ));
        assert_eq!(db.frontier().unwrap(), initial);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_none());
        assert_eq!(txn.outstanding_obligations(), baseline);
        scoped.limits.max_nodes = 4;
        let token = authority.issue_at(&scoped, NOW).unwrap();
        let seq = db
            .write_ordered_authorized(&txn, &cx, &authority, &token, "main", batches, || NOW)
            .await
            .unwrap();
        assert_eq!(seq.0, initial.0 + 1);
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}

async fn budget_attempt(
    contexts: &PurposeContexts,
    batches: Vec<WriteBatch>,
    work: u64,
    single: bool,
) -> Result<CommitSeq, WriteTxnError> {
    let cx = contexts.commit();
    let txn = contexts.txn();
    let baseline = txn.outstanding_obligations();
    let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
    let initial = db.frontier().unwrap();
    let authority = authority();
    let mut scoped = grant();
    scoped.limits.max_work = work;
    let token = authority.issue_at(&scoped, NOW).unwrap();
    let result = if single {
        assert_eq!(batches.len(), 1);
        db.write_authorized(
            &txn, &cx, &authority, &token, "main", batches.into_iter().next().unwrap(), || NOW,
        )
        .await
    } else {
        db.write_ordered_authorized(&txn, &cx, &authority, &token, "main", batches, || NOW)
            .await
    };
    assert_eq!(txn.outstanding_obligations(), baseline);
    if result.is_err() {
        assert_eq!(db.frontier().unwrap(), initial);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_none());
    }
    result
}

async fn minimum_work(contexts: &PurposeContexts, batches: &[WriteBatch], single: bool) -> u64 {
    let (mut low, mut high) = (0, 4096);
    budget_attempt(contexts, batches.to_vec(), high, single).await.unwrap();
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match budget_attempt(contexts, batches.to_vec(), middle, single).await {
            Ok(_) => high = middle,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(LimitDimension::Work))) => {
                low = middle;
            }
            other => panic!("unexpected budget outcome: {other:?}"),
        }
    }
    high
}

#[test]
fn splitting_batches_cannot_refresh_work_or_change_single_batch_charges() {
    under_lab(0xb305, |contexts| async move {
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(2), vec![L], vec![]);
        let split = vec![first.clone(), second.clone()];
        let mut grouped = first.clone();
        grouped.create_vertex(VId(2), vec![L], vec![]);
        let grouped = vec![grouped];
        let single = minimum_work(&contexts, &grouped, true).await;
        assert_eq!(minimum_work(&contexts, &grouped, false).await, single);
        assert_eq!(minimum_work(&contexts, &split, false).await, single);
        let first_only = minimum_work(&contexts, &[first], false).await;
        let second_only = minimum_work(&contexts, &[second], false).await;
        assert!(single > first_only.max(second_only));
        assert!(matches!(
            budget_attempt(&contexts, split, first_only.max(second_only), false).await,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(LimitDimension::Work)))
        ));
    });
}

#[test]
fn empty_groups_and_read_only_tokens_never_create_a_workspace_or_publish() {
    under_lab(0xb306, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
        let initial = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut malformed = dependent_batches();
        malformed.push(WriteBatch::new(T));
        for batches in [Vec::new(), vec![WriteBatch::new(R)], malformed] {
            assert!(matches!(
                db.write_ordered_authorized(&txn, &cx, &authority, &token, "main", batches, || NOW)
                    .await,
                Err(WriteTxnError::Write(WriteError::EmptyBatch))
            ));
            assert_eq!(db.frontier().unwrap(), initial);
            assert_eq!(txn.outstanding_obligations(), baseline);
            assert!(db.vertex(VId(1)).unwrap().is_none());
        }
        let mut read = grant();
        read.rights = Rights::Read;
        let token = authority.issue_at(&read, NOW).unwrap();
        assert!(matches!(
            db.write_ordered_authorized(&txn, &cx, &authority, &token, "main", Vec::new(), || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::PermissionDenied))
        ));
        assert_eq!(db.frontier().unwrap(), initial);
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}
