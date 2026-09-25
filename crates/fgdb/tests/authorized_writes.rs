//! Real Warden tokens over the public embedded write path and Chronicle reopen.
//! These are not image-validator fixtures: all observations come from Database.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::context::PurposeContexts;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, VId};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Rights, Scope};
use std::path::PathBuf;

#[path = "authorized_writes/graph.rs"]
mod graph;

const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x73; 32]);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const BRANCH: &str = "main";
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x17; 32], NAMESPACE, [0x19; 32])
}
fn issuer(namespace: DatabaseSecurityNamespaceId) -> Authority {
    Authority::new(
        AuthKey::from_seed(9701),
        namespace,
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: BRANCH.into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P]),
        rights: Rights::Write,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 0,
        },
        expires_at_ms: 10_000,
    }
}
/// The same write grant with nothing hidden: vertex deletion requires it.
fn total_grant() -> Grant {
    Grant {
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
        ..grant()
    }
}
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-authorized-writes-{}-{name}",
        std::process::id()
    ))
}
fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(
        report.lab_test_passed(),
        "lab invariants/quiescence: {report:?}"
    );
    result
}

#[test]
fn write_only_vertex_batch_commits_once_and_reopens() {
    under_lab(0xa901, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let path = scratch("vertex-reopen");
        let mut db = Database::create(&cx, &path, keys()).await.unwrap();
        let frontier = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(1))]);
        batch.create_vertex(VId(2), vec![L], vec![]);
        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
        batch.set_vertex_label(VId(2), L, true);
        let seq = db
            .write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
            .await
            .unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap().labels, vec![L]);
        drop(db);
        let reopened = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(
            reopened.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert!(reopened.vertex(VId(2)).unwrap().is_some());
    });
}

#[test]
fn forbidden_noop_tail_discards_the_entire_allowed_prefix() {
    under_lab(0xa902, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("denied-tail"), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(SECRET, CanonicalScalar::Int(99))]);
        db.write(&cx, seed).await.unwrap();
        let frontier = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(2), vec![L], vec![]);
        // A normalized no-op must not erase the attempted forbidden field.
        batch.set_vertex_property(VId(1), SECRET, Some(CanonicalScalar::Int(99)));
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txn.outstanding_obligations(), baseline);
        assert!(db.vertex(VId(2)).unwrap().is_none());
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(SECRET, CanonicalScalar::Int(99))]
        );
        // No failed workspace or identity reservation prevents a later write.
        let mut retry = WriteBatch::new(R);
        retry.create_vertex(VId(2), vec![L], vec![]);
        db.write_authorized(&txn, &cx, &authority, &token, BRANCH, retry, || NOW)
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}

#[test]
fn permitted_updates_preserve_hidden_fields_and_hide_missing_targets() {
    under_lab(0xa903, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("masked-update"), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(1)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(2))]);
        db.write(&cx, seed).await.unwrap();
        let authority = issuer(NAMESPACE);
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
        db.write_authorized(&txn, &cx, &authority, &token, BRANCH, update, || NOW)
            .await
            .unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(99))
            ]
        );
        let frontier = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        for vid in [VId(2), VId(999)] {
            let mut batch = WriteBatch::new(R);
            batch.set_vertex_property(vid, P, Some(CanonicalScalar::Int(5)));
            assert!(matches!(
                db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch, || NOW)
                    .await,
                Err(WriteTxnError::Authorization(Error::ScopeDenied))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txn.outstanding_obligations(), baseline);
        }
    });
}

#[test]
fn admission_and_shared_budget_refuse_without_publishing_or_leaking_pins() {
    under_lab(0xa904, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let mut db = Database::create(&cx, &scratch("admission"), keys())
            .await
            .unwrap();
        let frontier = db.frontier().unwrap();
        let baseline = txn.outstanding_obligations();
        let authority = issuer(NAMESPACE);
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![L], vec![]);
        let mut read = grant();
        read.rights = Rights::Read;
        let token = authority.issue_at(&read, NOW).unwrap();
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch.clone(), || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::PermissionDenied))
        ));
        let mut limited = grant();
        limited.limits.max_work = 1;
        let token = authority.issue_at(&limited, NOW).unwrap();
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, BRANCH, batch.clone(), || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::LimitExceeded(
                LimitDimension::Work
            )))
        ));
        let foreign = issuer(DatabaseSecurityNamespaceId([0x74; 32]));
        let token = foreign.issue_at(&grant(), NOW).unwrap();
        assert!(matches!(
            db.write_authorized(&txn, &cx, &foreign, &token, BRANCH, batch, || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::WrongAuthority))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}
