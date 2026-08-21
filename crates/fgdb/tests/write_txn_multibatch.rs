//! **WriteTxn: two writes, one capsule, one sequence**
//! (`fgdb-w4-g1-txn-core-qpmg.2`).
//!
//! A transaction is one atom no matter how many `write` calls staged it.
//! This suite pins the product meaning: two `WriteTxn::write` calls followed
//! by one `commit` consume EXACTLY ONE commit sequence and publish one
//! durable unit; an abort discards both staged batches together; and the FCW
//! verdict is rendered once per transaction, not once per staged batch.
//!
//! Compiles against the landed Wave-5 signatures (`write(&mut db, batch)`,
//! `commit(&mut db, &commit).await`, `abort(self)`) — the second `write`
//! call per txn is the capability this bead lands, so until it does, these
//! tests compile and FAIL (today's `write` refuses a second batch with
//! `AlreadyPrepared`). That red is the point; do not weaken it.
//!
//! **THE PLANTED NEGATIVE (test 1).** The tempting implementation of
//! "multiple writes" is a loop of autocommits: each staged batch becomes its
//! own commit at its own sequence, and every visibility assertion still
//! passes. The frontier arithmetic kills it: after seed (seq 1), one
//! two-write txn must land the frontier at EXACTLY seed+1 — two autocommits
//! land it at seed+2 and fail. Atomicity of abort (test 2) breaks the same
//! cheat from the other side: an autocommitting "write" has already
//! published its first batch when the abort arrives.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};
use std::path::PathBuf;

const KNOWS: RelationId = RelationId(1);
const PROP: PropertyKeyId = PropertyKeyId(7);
const PROP_B: PropertyKeyId = PropertyKeyId(9);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-txn-multi-{}-{name}", std::process::id()))
}

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

/// One live vertex `VId(1)` carrying `PROP = 0`: the overlap target for
/// test 3, and the untouched bystander for tests 1–2.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Two writes — create `VId(2)`, then set a property on it — one commit,
/// ONE sequence. The second batch touches a vertex that exists only in the
/// first staged batch, so staging must fold the txn's own prefix; and the
/// frontier arithmetic is the planted negative that fails a two-autocommit
/// counterfeit.
#[test]
fn two_writes_commit_as_one_sequence() {
    under_lab(0xb2_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("one-seq");
        {
            let mut db = seeded(&commit, &dir).await;
            let before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(VId(2), vec![LabelId(5)], vec![]);
            txn.write(&mut db, first).expect("first batch stages");
            let mut second = WriteBatch::new(KNOWS);
            second.set_vertex_property(VId(2), PROP, Some(int(7)));
            txn.write(&mut db, second)
                .expect("second batch stages against the txn's own prefix");
            let seq = txn
                .commit(&mut db, &commit)
                .await
                .expect("the two-write txn commits");

            assert_eq!(
                seq.0,
                before.0 + 1,
                "TWO writes, ONE commit sequence — a loop of autocommits \
                 lands at +2 and fails here"
            );
            assert_eq!(db.frontier().expect("healthy frontier"), seq);
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        let row = db.vertex(VId(2)).expect("reads").expect("created vertex");
        assert_eq!(row.labels, vec![LabelId(5)]);
        assert_eq!(
            row.props,
            vec![(PROP, int(7))],
            "the create AND the property ride one durable unit"
        );
    });
}

/// Abort discards BOTH staged batches: the frontier never moves and the
/// vertex the first batch created is nowhere across reopen. An
/// autocommitting "write" has already published batch one when the abort
/// arrives — this is the atomicity half of the planted negative.
#[test]
fn abort_discards_both_staged_batches() {
    under_lab(0xb2_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-both");
        let before;
        {
            let mut db = seeded(&commit, &dir).await;
            before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(VId(2), vec![LabelId(5)], vec![]);
            txn.write(&mut db, first).expect("first batch stages");
            let mut second = WriteBatch::new(KNOWS);
            second.set_vertex_property(VId(2), PROP, Some(int(7)));
            txn.write(&mut db, second).expect("second batch stages");
            txn.abort();

            assert_eq!(
                db.frontier().expect("healthy frontier"),
                before,
                "an aborted two-write txn consumed no sequence"
            );
            assert!(
                db.vertex(VId(2)).expect("reads").is_none(),
                "the staged create is invisible to the live fold"
            );
        }

        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(db.frontier().expect("healthy frontier"), before);
        assert!(
            db.vertex(VId(2)).expect("reads").is_none(),
            "neither staged batch left durable residue"
        );
    });
}

/// Two overlapping two-write txns against one basis, both updating `VId(1)`
/// properties: the first commit wins whole, the second loses whole with the
/// typed FCW abort — one verdict per TRANSACTION, so nothing of the loser's
/// two batches (not even its disjoint second write) survives.
#[test]
fn overlapping_two_write_txns_lose_whole_not_per_batch() {
    under_lab(0xb2_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("overlap-multi");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn_first = db.begin(&txn_cx).expect("first txn begins");
            let mut txn_second = db.begin(&txn_cx).expect("second txn begins at the same basis");

            let mut a1 = WriteBatch::new(KNOWS);
            a1.set_vertex_property(VId(1), PROP, Some(int(1)));
            txn_first.write(&mut db, a1).expect("winner stages batch one");
            let mut a2 = WriteBatch::new(KNOWS);
            a2.set_vertex_property(VId(1), PROP_B, Some(int(10)));
            txn_first.write(&mut db, a2).expect("winner stages batch two");

            let mut b1 = WriteBatch::new(KNOWS);
            b1.set_vertex_property(VId(1), PROP, Some(int(2)));
            txn_second.write(&mut db, b1).expect("loser stages batch one");
            let mut b2 = WriteBatch::new(KNOWS);
            b2.set_vertex_property(VId(1), PROP_B, Some(int(20)));
            txn_second.write(&mut db, b2).expect("loser stages batch two");

            txn_first
                .commit(&mut db, &commit)
                .await
                .expect("first committer wins");
            let err = txn_second
                .commit(&mut db, &commit)
                .await
                .expect_err("the overlapping second txn must lose whole");
            assert!(
                matches!(
                    err,
                    WriteTxnError::Write(WriteError::FirstCommitterWins { .. })
                ),
                "the loser must be the typed FCW arm, got {err:?}"
            );
            let rendered = format!("{err:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-01"),
                "the abort must name the FCW law: {rendered}"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        let row = db.vertex(VId(1)).expect("reads").expect("row");
        assert_eq!(
            row.props,
            vec![(PROP, int(1)), (PROP_B, int(10))],
            "the winner's TWO writes survive; nothing of the loser's does — \
             not even its second batch"
        );
    });
}
