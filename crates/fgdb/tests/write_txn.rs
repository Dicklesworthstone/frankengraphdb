//! **WriteTxn: the pinned-snapshot product transaction**
//! (`fgdb-writetxn-pin-l8wb`).
//!
//! Gap-2 remainder after FCW: `Database::begin` acquires the `TxnCx`
//! `pin_snapshot` obligation and records the frontier as the txn's basis;
//! `WriteTxn::write` prepares against that PINNED basis; `commit` goes
//! through the two-fsync path and releases the pin; `abort` releases the pin
//! with nothing durable. Not Graph-SSI, not the merge ladder, not
//! WriteCoordinator.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (landed at 9048fc5):
//! - `Database::begin(&mut self, txn: &TxnCx) -> Result<WriteTxn, WriteError>`
//! - `WriteTxn::write(&mut self, &mut Database<V>, WriteBatch)` (one batch
//!   per txn this slice)
//! - `WriteTxn::commit(&mut self, &mut Database<V>, &CommitCx)` (async),
//!   returning `WriteTxnError`; the FCW loser surfaces as
//!   `WriteTxnError::Write(WriteError::FirstCommitterWins { .. })`
//! - `WriteTxn::abort(self)`
//!
//! **THE PLANTED NEGATIVE (test 3).** The cheap counterfeit is a `begin`
//! that stores a basis number and never touches `TxnCx::pin_snapshot`: every
//! commit/abort test still passes, but the pin is fiction — nothing in the
//! runtime knows a snapshot is held, so nothing can hold compaction or lab
//! oracles to it. The obligation ledger is the observable:
//! `outstanding_obligations()` on the very `TxnCx` handed to `begin` MUST
//! rise while the txn is open and return to its baseline after commit AND
//! after abort. A pin that was never acquired cannot raise it; a pin that is
//! never released cannot lower it back.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError, WriteTxn, WriteTxnError};
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
    std::env::temp_dir().join(format!("fgdb-write-txn-{}-{name}", std::process::id()))
}

/// Unlike the sibling suites this harness hands the test the WHOLE
/// `PurposeContexts`: a txn test needs the `TxnCx` (to begin and to read the
/// obligation ledger) and the `CommitCx` (to commit) as two separately
/// narrowed capabilities, exactly as a session would hold them.
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

/// Seed one live vertex carrying `PROP = 0`, so every conflict below is a
/// pure property-family update — the shape only FCW can refuse.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Two txns begun against one basis, overlapping property updates on
/// `VId(1)`: the first commit wins, the second receives the typed
/// `WriteError::FirstCommitterWins` abort, and a cold reopen serves only the
/// winner. The obligation ledger returns to baseline after both outcomes —
/// the loser's pin is released by its failed commit, not leaked.
#[test]
fn overlapping_txns_first_commit_wins_second_aborts_typed() {
    under_lab(0x7a_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("overlap");
        {
            let mut db = seeded(&commit, &dir).await;
            let baseline = txn_cx.outstanding_obligations();

            let mut txn_first = db.begin(&txn_cx).expect("first txn begins");
            let mut txn_second = db
                .begin(&txn_cx)
                .expect("second txn begins at the same basis");

            let mut winner = WriteBatch::new(KNOWS);
            winner.set_vertex_property(VId(1), PROP, Some(int(1)));
            txn_first
                .write(&mut db, winner)
                .expect("winner stages its batch");
            let mut loser = WriteBatch::new(KNOWS);
            loser.set_vertex_property(VId(1), PROP, Some(int(2)));
            txn_second
                .write(&mut db, loser)
                .expect("loser stages its batch");

            txn_first
                .commit(&mut db, &commit)
                .await
                .expect("first committer wins");
            let err = txn_second
                .commit(&mut db, &commit)
                .await
                .expect_err("the overlapping second txn must lose");
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
            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline,
                "both pins are released: the winner's by commit, the loser's \
                 by its failed commit — a leaked pin here outlives its txn"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(1))],
            "only the winning txn's property survives the reopen"
        );
    });
}

/// begin → write → abort: the obligation ledger returns to its baseline,
/// nothing of the aborted write is durable, and the handle keeps working —
/// an autocommit write after the abort commits and survives reopen.
#[test]
fn abort_releases_the_pin_and_leaves_nothing_durable() {
    under_lab(0x7a_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort");
        {
            let mut db = seeded(&commit, &dir).await;
            let baseline = txn_cx.outstanding_obligations();
            let frontier_before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.set_vertex_property(VId(1), PROP, Some(int(99)));
            txn.write(&mut db, batch).expect("stages its batch");
            txn.abort();

            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline,
                "abort must release the pin"
            );
            assert_eq!(
                db.frontier().expect("healthy frontier"),
                frontier_before,
                "an aborted txn consumes no sequence"
            );
            assert_eq!(
                db.vertex(VId(1)).expect("reads").expect("row").props,
                vec![(PROP, int(0))],
                "the aborted write is invisible to the live fold"
            );

            let mut after = WriteBatch::new(KNOWS);
            after.set_vertex_property(VId(1), PROP, Some(int(5)));
            db.write(&commit, after)
                .await
                .expect("autocommit after abort works");
        }

        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(5))],
            "reopen holds the seed and the post-abort write; the aborted \
             txn's 99 is nowhere"
        );
    });
}

/// THE PLANTED NEGATIVE, live: while a txn is open — after `begin`, before
/// commit or abort — the `TxnCx` obligation ledger is ABOVE its baseline. A
/// `begin` that skipped `TxnCx::pin_snapshot` (storing a bare basis number
/// instead) passes every other test in this file and fails this one, because
/// no bookkeeping it invents can raise the runtime's own ledger.
#[test]
fn an_open_txn_holds_a_live_pin_obligation() {
    under_lab(0x7a_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("live-pin");
        let mut db = seeded(&commit, &dir).await;

        let baseline = txn_cx.outstanding_obligations();
        let txn = db.begin(&txn_cx).expect("txn begins");
        assert!(
            txn_cx.outstanding_obligations() > baseline,
            "begin must ACQUIRE the pin_snapshot obligation: ledger stayed at \
             {baseline}, so the \"pinned\" snapshot is fiction"
        );
        let held: &WriteTxn = &txn;
        let _ = held; // the obligation belongs to this txn value, still alive here
        txn.abort();
        assert_eq!(
            txn_cx.outstanding_obligations(),
            baseline,
            "the ledger returns to baseline once the txn ends"
        );
    });
}
