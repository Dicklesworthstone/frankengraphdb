//! **WriteTxn under injected crash: the pin dies with the attempt**
//! (`fgdb-writetxn-crash-k6cw`).
//!
//! `WriteTxn::commit` releases the `pin_snapshot` obligation on every exit;
//! this suite proves the crash-injected twin keeps that law on BOTH of its
//! exits. `commit_with_crash(.., None)` must be indistinguishable from
//! `commit` — seq advances, ledger returns to baseline, the write is durable
//! across reopen. `commit_with_crash(.., Some(BeforeCapsule))` must fail
//! BEFORE anything durable exists — and still return the ledger to baseline.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (this wave's landed names):
//! - `WriteTxn::commit_with_crash(&mut self, &mut Database<V>, &CommitCx,
//!    Option<CrashPoint>)` (async) → same `Result<CommitSeq, WriteTxnError>`
//!   shape as `commit`.
//! Until it lands this file fails to compile — deliberately; do not weaken
//! it to make it compile.
//!
//! **THE PLANTED NEGATIVE (test 2).** The tempting implementation is
//! `commit_with_crash` as a thin fork of `commit` whose error path `?`s the
//! injected crash straight out — past `release_pin`. Every durability
//! assertion still passes (a BeforeCapsule crash IS residue-free), but the
//! pin outlives its transaction: the ledger stays above baseline forever,
//! and under the lab runtime that is a leaked obligation the quiescence
//! oracle may also flag. The ledger assertion AFTER the error return is the
//! test only that cheat fails.

use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};
use std::path::PathBuf;

const KNOWS: RelationId = RelationId(1);
const PROP: PropertyKeyId = PropertyKeyId(7);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-txn-crash-{}-{name}", std::process::id()))
}

/// Hands the test the whole `PurposeContexts`: this suite needs the `TxnCx`
/// (begin + obligation ledger) and the `CommitCx` (commit) separately.
fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value.into())
}

/// One live vertex carrying `PROP = 0`: the value every crash test below
/// must find UNCHANGED, and the row the clean-path test must see updated.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// `commit_with_crash(.., None)` IS `commit`: the seq advances past the
/// basis, the pin ledger returns to baseline, and the write survives a cold
/// reopen. Injection disabled must be the identity, or every crash result
/// below is about a different commit path than the product runs.
#[test]
fn crash_disabled_commits_exactly_like_commit() {
    under_lab(0xcc_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("no-crash");
        {
            let mut db = seeded(&commit, &dir).await;
            let baseline = txn_cx.outstanding_obligations();
            let frontier_before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.set_vertex_property(VId(1), PROP, Some(int(1)));
            txn.write(&mut db, batch).expect("stages its batch");
            let seq = txn
                .commit_with_crash(&mut db, &commit, None)
                .await
                .expect("no injection: the commit is ordinary");

            assert!(
                seq > frontier_before,
                "the commit consumed a sequence past the basis: {frontier_before:?} -> {seq:?}"
            );
            assert_eq!(
                db.frontier().expect("healthy frontier"),
                seq,
                "the handle's frontier is the committed seq"
            );
            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline,
                "the pin is released on the success path"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(1))],
            "the committed write is durable"
        );
    });
}

/// THE PLANTED NEGATIVE, live: a `BeforeCapsule` crash makes
/// `commit_with_crash` return `Err` — and the pin ledger must STILL return
/// to baseline. An implementation that `?`s the injected failure out before
/// `release_pin` passes every durability assertion in this file and fails
/// exactly this one: no bookkeeping it invents can lower the runtime's own
/// obligation ledger.
#[test]
fn a_crashed_commit_still_releases_the_pin() {
    under_lab(0xcc_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("crash-pin");
        let mut db = seeded(&commit, &dir).await;

        let baseline = txn_cx.outstanding_obligations();
        let mut txn = db.begin(&txn_cx).expect("txn begins");
        assert!(
            txn_cx.outstanding_obligations() > baseline,
            "control: the open txn holds a live pin — without this, the \
             baseline equality below would be vacuously green"
        );
        let mut batch = WriteBatch::new(KNOWS);
        batch.set_vertex_property(VId(1), PROP, Some(int(99)));
        txn.write(&mut db, batch).expect("stages its batch");

        txn.commit_with_crash(&mut db, &commit, Some(CrashPoint::BeforeCapsule))
            .await
            .expect_err("BeforeCapsule: the commit must fail");
        assert_eq!(
            txn_cx.outstanding_obligations(),
            baseline,
            "the pin must die with the failed attempt — an error path that \
             skips release_pin leaks the obligation past its transaction"
        );
    });
}

/// A `BeforeCapsule` crash is durably invisible: after drop and cold reopen,
/// the seed property is unchanged and the frontier never moved — the crashed
/// attempt consumed no sequence and left no bytes.
#[test]
fn a_before_capsule_crash_is_residue_free_across_reopen() {
    under_lab(0xcc_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("crash-residue");
        let frontier_before;
        {
            let mut db = seeded(&commit, &dir).await;
            frontier_before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.set_vertex_property(VId(1), PROP, Some(int(99)));
            txn.write(&mut db, batch).expect("stages its batch");
            txn.commit_with_crash(&mut db, &commit, Some(CrashPoint::BeforeCapsule))
                .await
                .expect_err("BeforeCapsule: the commit must fail");
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.frontier().expect("healthy frontier"),
            frontier_before,
            "nothing before the capsule is durable: the crashed attempt \
             consumed no sequence"
        );
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(0))],
            "the seed value is untouched; the crashed write's 99 is nowhere"
        );
    });
}
