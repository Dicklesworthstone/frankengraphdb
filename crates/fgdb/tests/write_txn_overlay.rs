//! **WriteTxn read-your-writes: the overlay is the txn's, not the world's**
//! (`fgdb-w4-g1-txn-core-qpmg.3`).
//!
//! A transaction that cannot see its own staged writes forces every caller
//! into bookkeeping the engine already did; a transaction whose staged
//! writes LEAK into the shared handle before commit is worse — that is
//! dirty read, and no isolation claim survives it. This suite pins both
//! directions at once, every time: whenever the overlay shows a staged
//! effect, the same assertion block shows the shared `Database` handle NOT
//! showing it.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (this wave's landed name):
//! - `WriteTxn::vertex(&self, &Database<V>, VId)` → the same
//!   `Option<VertexRow>`-shaped answer `Database::vertex` gives, folded as
//!   pinned basis + this txn's staged batches.
//!
//! Until it lands this file fails to compile — deliberately; do not weaken
//! it to make it compile.
//!
//! **WHAT WOULD MAKE THIS VACUOUS.** An overlay implemented by applying the
//! staged batch to the SHARED fold would pass every "overlay sees it"
//! assertion — so each test's load-bearing line is the pairing: overlay
//! `Some`, base handle `None` (or old value), at the same instant. And an
//! overlay that answers from the live frontier instead of the pinned basis
//! is indistinguishable here only while nothing else commits — the basis
//! discipline already has its own suite (`SnapshotAdvanced`); this one owns
//! the staged-visibility law.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
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
    std::env::temp_dir().join(format!("fgdb-txn-overlay-{}-{name}", std::process::id()))
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
    CanonicalScalar::Int(value)
}

/// One live vertex `VId(1)` carrying `PROP = 0`: the update target for
/// test 3 and the proof the overlay serves BASE state too, not only deltas.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// A staged create is visible to ITS transaction and invisible to the
/// shared handle — at the same instant — then durable for everyone after
/// commit. The paired assertions are the law: either half alone is
/// satisfiable by a broken engine.
#[test]
fn staged_create_is_txn_visible_and_base_invisible_until_commit() {
    under_lab(0xd0_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-create");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(2), vec![LabelId(5)], vec![(PROP, int(7))]);
            txn.write(&mut db, batch).expect("stages the create");

            let overlay = txn
                .vertex(&db, VId(2))
                .expect("overlay reads")
                .expect("the txn sees its own staged create");
            assert_eq!(overlay.labels, vec![LabelId(5)]);
            assert_eq!(overlay.props, vec![(PROP, int(7))]);
            assert!(
                db.vertex(VId(2)).expect("base reads").is_none(),
                "DIRTY READ: the staged create leaked into the shared handle \
                 before commit"
            );
            // The overlay serves base state too — it is basis + staged, not
            // a bare delta map.
            let bystander = txn
                .vertex(&db, VId(1))
                .expect("overlay reads the base")
                .expect("committed state is visible through the overlay");
            assert_eq!(bystander.props, vec![(PROP, int(0))]);

            txn.commit(&mut db, &commit).await.expect("commits");
            assert!(
                db.vertex(VId(2)).expect("base reads").is_some(),
                "after commit the shared handle serves the vertex"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        let row = db.vertex(VId(2)).expect("reads").expect("durable vertex");
        assert_eq!(row.labels, vec![LabelId(5)]);
        assert_eq!(row.props, vec![(PROP, int(7))]);
    });
}

/// Abort throws the staged create away everywhere: the base handle never
/// saw it, and after abort nobody ever will.
#[test]
fn aborted_staged_create_is_nowhere() {
    under_lab(0xd0_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-create");
        let mut db = seeded(&commit, &dir).await;

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(2), vec![LabelId(5)], vec![(PROP, int(7))]);
        txn.write(&mut db, batch).expect("stages the create");
        assert!(
            txn.vertex(&db, VId(2)).expect("overlay reads").is_some(),
            "control: the staged create was visible to its txn — without \
             this, the None below would be vacuously green"
        );
        txn.abort();

        assert!(
            db.vertex(VId(2)).expect("base reads").is_none(),
            "the aborted create is nowhere"
        );
    });
}

/// A staged UPDATE on a live vertex: the overlay serves the new value, the
/// shared handle keeps the old one, and after abort the old value is still
/// the only value — live and across reopen.
#[test]
fn staged_update_overlays_without_leaking_and_abort_restores_nothing() {
    under_lab(0xd0_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-update");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.set_vertex_property(VId(1), PROP, Some(int(9)));
            txn.write(&mut db, batch).expect("stages the update");

            let overlay = txn
                .vertex(&db, VId(1))
                .expect("overlay reads")
                .expect("the live vertex is visible through the overlay");
            assert_eq!(
                overlay.props,
                vec![(PROP, int(9))],
                "the overlay serves the staged value"
            );
            assert_eq!(
                db.vertex(VId(1)).expect("base reads").expect("row").props,
                vec![(PROP, int(0))],
                "DIRTY READ: the staged update leaked into the shared handle"
            );

            txn.abort();
            assert_eq!(
                db.vertex(VId(1)).expect("base reads").expect("row").props,
                vec![(PROP, int(0))],
                "abort restores nothing because nothing ever changed"
            );
        }

        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(0))],
            "the staged 9 never became durable"
        );
    });
}
