//! **Overlay reads form a commit-time read-set**
//! (`fgdb-w4-g1-txn-core-qpmg.5`).
//!
//! `WriteTxn::vertex` gave the transaction read-your-writes; this suite
//! makes those reads COUNT: an element a transaction read is part of what
//! its commit certifies, so a concurrent committer that overwrote it aborts
//! the reader's commit — even when the two WRITE-sets never touch. That
//! last clause is the whole design of test 1: the loser's staged write is
//! DISJOINT from the winner's, so the write-set-only FCW validator sees no
//! conflict and the abort can come from exactly one place — the recorded
//! read-set.
//!
//! **THE PLANTED NEGATIVE (test 3, living inside test 1).** A
//! `WriteTxn::vertex` that answers correctly but records nothing passes
//! every overlay-visibility test ever written and commits test 1's loser —
//! publishing a decision derived from a value that was already dead. No
//! separate test can catch that cheat; only a conflict invisible to the
//! write-set can, and test 1 is that conflict. Test 2 is the other guard
//! rail: a read-set that aborts on ANY concurrent commit (not just one
//! overlapping the reads) is a serial-execution lock wearing a validator's
//! name, and it fails test 2's disjoint case.
//!
//! **Law identity.** The abort must carry `FG-LAW-FCW-READ-01` in its
//! rendering (the read-set refinement of FG-LAW-FCW-01; a new typed arm or
//! the FirstCommitterWins arm both satisfy this file, a generic
//! `Commit`-wrapped error does not).

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
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
    std::env::temp_dir().join(format!("fgdb-txn-readset-{}-{name}", std::process::id()))
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

/// One live vertex `VId(1)` carrying `PROP = 0`: the element txn A reads
/// and txn B overwrites.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// A read, then a concurrent overwrite of the READ element, then the
/// reader's commit of a DISJOINT write: typed abort naming
/// FG-LAW-FCW-READ-01. Write-sets never overlap — only the recorded
/// read-set can deliver this verdict, which is the planted negative for a
/// `vertex()` that answers without recording.
#[test]
fn a_commit_whose_read_was_overwritten_aborts_typed() {
    under_lab(0xf0_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("read-overwritten");
        {
            let mut db = seeded(&commit, &dir).await;

            // Txn A reads VId(1) — the value its later write is "based on" —
            // and stages a write touching ONLY VId(3).
            let mut txn_a = db.begin(&txn_cx).expect("reader txn begins");
            let observed = txn_a
                .vertex(&db, VId(1))
                .expect("overlay reads")
                .expect("the seeded vertex is visible");
            assert_eq!(observed.props, vec![(PROP, int(0))]);
            let mut disjoint = WriteBatch::new(KNOWS);
            disjoint.create_vertex(VId(3), vec![LabelId(5)], vec![(PROP, int(3))]);
            txn_a.write(&mut db, disjoint).expect("stages the disjoint write");

            // Txn B overwrites the element A READ, and wins.
            let mut txn_b = db.begin(&txn_cx).expect("writer txn begins");
            let mut overwrite = WriteBatch::new(KNOWS);
            overwrite.set_vertex_property(VId(1), PROP, Some(int(1)));
            txn_b.write(&mut db, overwrite).expect("stages the overwrite");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the concurrent writer commits first");

            // A's write-set {VId(3)} does not overlap B's {VId(1)}: only the
            // READ-set can abort this commit. A vertex() that recorded
            // nothing commits here — publishing a decision based on a value
            // that was already dead — and this expect_err is what fails.
            let err = txn_a
                .commit(&mut db, &commit)
                .await
                .expect_err("the reader's basis observation is dead; commit must abort");
            assert!(
                !matches!(err, WriteTxnError::Write(WriteError::Commit(_))),
                "the abort must be typed, not the generic Commit wrap: {err:?}"
            );
            let rendered = format!("{err:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "the abort must name the read-set law: {rendered}"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(1))],
            "the winner's overwrite is durable"
        );
        assert!(
            db.vertex(VId(3)).expect("reads").is_none(),
            "nothing of the aborted reader's txn is durable — its disjoint \
             create died with its commit"
        );
    });
}

/// The guard rail against over-abort: the concurrent commit touched NOTHING
/// txn A read or wrote, so A commits fine. A "read-set" that aborts on any
/// concurrent commit is a serial lock, and this test is what it fails.
#[test]
fn a_disjoint_concurrent_commit_does_not_abort_the_reader() {
    under_lab(0xf0_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("disjoint-ok");
        {
            let mut db = seeded(&commit, &dir).await;

            // Txn A reads VId(1) and stages an update of the SAME vertex —
            // read-set and write-set both {VId(1)}.
            let mut txn_a = db.begin(&txn_cx).expect("reader txn begins");
            let observed = txn_a
                .vertex(&db, VId(1))
                .expect("overlay reads")
                .expect("the seeded vertex is visible");
            assert_eq!(observed.props, vec![(PROP, int(0))]);
            let mut update = WriteBatch::new(KNOWS);
            update.set_vertex_property(VId(1), PROP, Some(int(5)));
            txn_a.write(&mut db, update).expect("stages the update");

            // Txn B creates VId(2) — disjoint from everything A touched.
            let mut txn_b = db.begin(&txn_cx).expect("creator txn begins");
            let mut create = WriteBatch::new(KNOWS);
            create.create_vertex(VId(2), vec![], vec![(PROP, int(2))]);
            txn_b.write(&mut db, create).expect("stages the create");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the disjoint concurrent txn commits");

            txn_a.commit(&mut db, &commit).await.expect(
                "nothing A read or wrote was touched — an abort here is \
                 over-approximation, not validation",
            );
        }

        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(5))],
            "the reader's update is durable"
        );
        assert_eq!(
            db.vertex(VId(2)).expect("reads").expect("row").props,
            vec![(PROP, int(2))],
            "the concurrent create is durable beside it — both committed"
        );
    });
}
