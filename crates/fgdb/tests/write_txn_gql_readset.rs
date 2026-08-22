//! **MATCH destinations join the commit-time read-set**
//! (`fgdb-w4-g1-txn-core-qpmg.6`).
//!
//! A transaction that ran `MATCH (a)-[:R]->(b) RETURN b` DERIVED something
//! from every destination it returned; committing a decision built on those
//! rows after a concurrent writer changed one of them is the same lost
//! basis as an overwritten point read. This suite extends the read-set law
//! from `write_txn_readset.rs` to the language surface: the matched
//! destination is read-state, and a concurrent overwrite of it aborts the
//! matcher's commit.
//!
//! **CONSTRUCTION, same as the point-read suite.** The matcher's staged
//! write touches ONLY `VId(3)` — disjoint from everything the concurrent
//! writer commits — so the write-set FCW validator sees no conflict and the
//! abort can come from exactly one place: the MATCH rows recorded into the
//! read-set. An `execute_gql` overlay that answers without recording
//! commits the loser and fails test 1's `expect_err`; a read-set that
//! aborts on ANY concurrent commit is a serial lock and fails test 2's
//! bare-vertex control (a created vertex with no `:R` edge changes no MATCH
//! row, so it invalidates nothing the matcher read).

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
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
    std::env::temp_dir().join(format!("fgdb-gql-readset-{}-{name}", std::process::id()))
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

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value.into())
}

/// One committed `:R` edge `VId(1) -> VId(2)`, `VId(2)` carrying
/// `PROP = 0`: the destination the MATCH returns and the writer overwrites.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![(PROP, int(0))]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// The MATCHed destination is overwritten by a concurrent committer: the
/// matcher's commit — whose write-set is disjoint — must abort typed,
/// rendering FG-LAW-FCW-READ-01, and reopen holds the winner's property
/// with nothing of the matcher's txn durable.
#[test]
fn a_commit_whose_matched_destination_was_overwritten_aborts_typed() {
    under_lab(0xa1_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("dest-overwritten");
        {
            let mut db = seeded(&commit, &dir).await;

            // Txn A matches — VId(2) enters its read-set — and stages a
            // write touching ONLY VId(3).
            let mut txn_a = db.begin(&txn_cx).expect("matcher txn begins");
            let matched = txn_a
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(matched, vec![VId(2)], "the seeded destination is matched");
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![LabelId(5)], vec![(PROP, int(3))]);
            txn_a
                .write(&mut db, disjoint)
                .expect("stages the disjoint write");

            // Txn B overwrites the MATCHED destination, and wins.
            let mut txn_b = db.begin(&txn_cx).expect("writer txn begins");
            let mut overwrite = WriteBatch::new(R);
            overwrite.set_vertex_property(VId(2), PROP, Some(int(1)));
            txn_b
                .write(&mut db, overwrite)
                .expect("stages the overwrite");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the concurrent writer commits first");

            // Write-sets: A={VId(3)}, B={VId(2)} — no overlap. Only the
            // MATCH rows recorded as reads can abort this commit; an
            // overlay MATCH that recorded nothing commits the loser here.
            let err = txn_a
                .commit(&mut db, &commit)
                .await
                .expect_err("the matched basis is dead; the commit must abort");
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
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(2)).expect("reads").expect("row").props,
            vec![(PROP, int(1))],
            "the winner's overwrite is durable"
        );
        assert!(
            db.vertex(VId(3)).expect("reads").is_none(),
            "nothing of the aborted matcher's txn is durable"
        );
    });
}

/// The guard rail: a concurrent commit that creates a bare vertex — no `:R`
/// edge, so no MATCH row changed — must not abort the matcher. Both txns'
/// effects are durable side by side across reopen.
#[test]
fn a_disjoint_concurrent_create_does_not_abort_the_matcher() {
    under_lab(0xa1_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("disjoint-ok");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn_a = db.begin(&txn_cx).expect("matcher txn begins");
            let matched = txn_a
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(matched, vec![VId(2)]);
            let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(3), vec![LabelId(5)], vec![(PROP, int(3))]);
            txn_a.write(&mut db, staged).expect("stages its write");

            // Txn B creates a bare VId(9): not matched, not readable through
            // any row A derived from — nothing A read changed.
            let mut txn_b = db.begin(&txn_cx).expect("creator txn begins");
            let mut create = WriteBatch::new(R);
            create.create_vertex(VId(9), vec![], vec![(PROP, int(9))]);
            txn_b
                .write(&mut db, create)
                .expect("stages the bare create");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the disjoint concurrent txn commits");

            txn_a.commit(&mut db, &commit).await.expect(
                "no MATCH row A returned was touched — an abort here is a \
                 serial lock, not read-set validation",
            );
        }

        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(3)).expect("reads").expect("row").props,
            vec![(PROP, int(3))],
            "the matcher's create is durable"
        );
        assert_eq!(
            db.vertex(VId(9)).expect("reads").expect("row").props,
            vec![(PROP, int(9))],
            "the concurrent bare create is durable beside it — both committed"
        );
    });
}
