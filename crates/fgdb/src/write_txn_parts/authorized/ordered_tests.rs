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
    Authority::new(
        AuthKey::from_seed(0xb301),
        NAMESPACE,
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
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
    assert!(
        report.lab_test_passed(),
        "lab invariants/quiescence: {report:?}"
    );
}

/// The trusted host seeds vertices 1 (P=1), 2 and 3 and one edge per relation:
/// 10 (R, 1->2, P=10), 20 (S, 2->3) and 30 (T, 3->1). Authorized batches
/// address existing identities only (fgdb-hxgm1).
async fn seed_graph<V: Vfs + Clone>(db: &mut Database<V>, cx: &fgdb_types::CommitCx) {
    let mut vertices = WriteBatch::new(R);
    vertices.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
    vertices.create_vertex(VId(2), vec![L], vec![]);
    vertices.create_vertex(VId(3), vec![L], vec![]);
    vertices.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(10))]);
    db.write(cx, vertices).await.unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(20), VId(2), VId(3), vec![]);
    db.write(cx, second).await.unwrap();
    let mut third = WriteBatch::new(T);
    third.add_edge(EId(30), VId(3), VId(1), vec![]);
    db.write(cx, third).await.unwrap();
}

/// Cross-relation batches that depend on each other: S's compare-and-set
/// needs R's write, and T edits an R edge by identity.
fn dependent_batches() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(R);
    first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2)));
    let mut second = WriteBatch::new(S);
    second.compare_and_set_vertex_property(
        VId(1),
        P,
        Some(CanonicalScalar::Int(2)),
        CanonicalScalar::Int(3),
        WriteMismatchPolicy::AbortWrite,
    );
    let mut third = WriteBatch::new(T);
    // Identity-addressed mutations must retain the actual R coordinate.
    third.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(11)));
    vec![first, second, third]
}

/// The seeded graph, with `vertex_p` on vertex 1 and `edge_p` on edge 10.
fn assert_graph<V: Vfs + Clone>(db: &Database<V>, seq: CommitSeq, vertex_p: i64, edge_p: i64) {
    assert_eq!(db.frontier().unwrap(), seq);
    assert_eq!(
        db.vertex(VId(1)).unwrap().unwrap().props,
        vec![(P, CanonicalScalar::Int(vertex_p))]
    );
    for vid in [VId(1), VId(2), VId(3)] {
        assert_eq!(db.vertex(vid).unwrap().unwrap().labels, vec![L]);
    }
    assert_eq!(
        db.edge_at(EId(10), seq).unwrap().unwrap().props,
        vec![(P, CanonicalScalar::Int(edge_p))]
    );
    assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(2)]);
    assert_eq!(db.neighbours(VId(2), S).unwrap(), vec![VId(3)]);
    assert_eq!(db.neighbours(VId(3), T).unwrap(), vec![VId(1)]);
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
        seed_graph(&mut db, &cx).await;
        let initial = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let seq = db
            .write_ordered_authorized(
                &txn,
                &cx,
                &authority,
                &token,
                "main",
                dependent_batches(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(seq.0, initial.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_graph(&db, seq, 3, 11);
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_graph(&reopened, seq, 3, 11);
    });
}

#[test]
fn forbidden_noop_in_another_relation_discards_all_prefix_effects() {
    under_lab(0xb302, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
        seed_graph(&mut db, &cx).await;
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
        // Every prefix effect in every relation was discarded.
        assert_graph(&db, initial, 1, 10);
        assert_eq!(
            db.vertex(VId(9)).unwrap().unwrap().props,
            vec![(SECRET, CanonicalScalar::Int(99))]
        );
        // The failed workspace must not fence the handle.
        let seq = db
            .write_ordered_authorized(
                &txn,
                &cx,
                &authority,
                &token,
                "main",
                dependent_batches(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(seq.0, initial.0 + 1);
        assert_graph(&db, seq, 3, 11);
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
        // An allowed prefix that the refused tail must discard.
        let mut first = WriteBatch::new(R);
        first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        let mut second = WriteBatch::new(S);
        second.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(2)));
        assert!(matches!(
            db.write_ordered_authorized(
                &txn,
                &cx,
                &authority,
                &token,
                "main",
                vec![first, second],
                || NOW,
            )
            .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), initial);
        assert!(db.vertex(VId(1)).unwrap().unwrap().props.is_empty());
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
        // Each update pays node admissions for its original images. Measure
        // each batch alone and both together: one allowance spans the
        // relation boundary, so together they need more than either alone.
        let nodes = LimitDimension::Nodes;
        let mut first = WriteBatch::new(R);
        first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        let mut second = WriteBatch::new(S);
        second.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(6)));
        let first_only = minimum(&contexts, &[first.clone()], nodes, false).await;
        let second_only = minimum(&contexts, &[second.clone()], nodes, false).await;
        let both = minimum(&contexts, &[first.clone(), second.clone()], nodes, false).await;
        assert!(first_only > 0 && second_only > 0, "updates must pay nodes");
        assert!(
            both > first_only.max(second_only),
            "{both} vs {first_only}/{second_only}"
        );
        assert!(matches!(
            budget_attempt(
                &contexts,
                vec![first, second],
                nodes,
                first_only.max(second_only),
                false
            )
            .await,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(
                LimitDimension::Nodes
            )))
        ));
    });
}

/// One attempt on a freshly seeded graph with `limit` on `dimension` (work or
/// nodes); a refusal must leave the seeded graph exactly as it was.
async fn budget_attempt(
    contexts: &PurposeContexts,
    batches: Vec<WriteBatch>,
    dimension: LimitDimension,
    limit: u64,
    single: bool,
) -> Result<CommitSeq, WriteTxnError> {
    let cx = contexts.commit();
    let txn = contexts.txn();
    let baseline = txn.outstanding_obligations();
    let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
    seed_graph(&mut db, &cx).await;
    let initial = db.frontier().unwrap();
    let authority = authority();
    let mut scoped = grant();
    if matches!(dimension, LimitDimension::Nodes) {
        scoped.limits.max_nodes = limit;
    } else {
        scoped.limits.max_work = limit;
    }
    let token = authority.issue_at(&scoped, NOW).unwrap();
    let result = if single {
        assert_eq!(batches.len(), 1);
        db.write_authorized(
            &txn,
            &cx,
            &authority,
            &token,
            "main",
            batches.into_iter().next().unwrap(),
            || NOW,
        )
        .await
    } else {
        db.write_ordered_authorized(&txn, &cx, &authority, &token, "main", batches, || NOW)
            .await
    };
    assert_eq!(txn.outstanding_obligations(), baseline);
    if result.is_err() {
        assert_graph(&db, initial, 1, 10);
    }
    result
}

/// The smallest `dimension` limit at which `batches` commit (the grant's own
/// allowance, 4096 work or 100 nodes, must suffice).
async fn minimum(
    contexts: &PurposeContexts,
    batches: &[WriteBatch],
    dimension: LimitDimension,
    single: bool,
) -> u64 {
    let ceiling = if matches!(dimension, LimitDimension::Nodes) {
        100
    } else {
        4096
    };
    let (mut low, mut high) = (0, ceiling);
    budget_attempt(contexts, batches.to_vec(), dimension, high, single)
        .await
        .unwrap();
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match budget_attempt(contexts, batches.to_vec(), dimension, middle, single).await {
            Ok(_) => high = middle,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(found)))
                if found == dimension =>
            {
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
        let work = LimitDimension::Work;
        let mut first = WriteBatch::new(R);
        first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        let mut second = WriteBatch::new(R);
        second.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(6)));
        let split = vec![first.clone(), second.clone()];
        let mut grouped = first.clone();
        grouped.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(6)));
        let grouped = vec![grouped];
        let single = minimum(&contexts, &grouped, work, true).await;
        assert_eq!(minimum(&contexts, &grouped, work, false).await, single);
        assert_eq!(minimum(&contexts, &split, work, false).await, single);
        let first_only = minimum(&contexts, &[first], work, false).await;
        let second_only = minimum(&contexts, &[second], work, false).await;
        assert!(single > first_only.max(second_only));
        assert!(matches!(
            budget_attempt(&contexts, split, work, first_only.max(second_only), false).await,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(
                LimitDimension::Work
            )))
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
        // A creation in ANY batch refuses the whole write before a workspace
        // exists or an earlier batch is staged (fgdb-hxgm1). The earlier
        // batches name vertices that do not even exist here.
        let mut create = WriteBatch::new(S);
        create.create_vertex(VId(1), vec![L], vec![]);
        let mut batches = dependent_batches();
        batches.push(create);
        assert!(matches!(
            db.write_ordered_authorized(&txn, &cx, &authority, &token, "main", batches, || NOW)
                .await,
            Err(WriteTxnError::AuthorizedClientIdentity)
        ));
        assert_eq!(db.frontier().unwrap(), initial);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert!(db.vertex(VId(1)).unwrap().is_none());
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
