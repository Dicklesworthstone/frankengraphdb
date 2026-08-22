//! **The incoming MATCH through the transaction overlay**
//! (`fgdb-gql-incoming-1qei`, overlay + as-of slice).
//!
//! `gql_incoming.rs` proved the `(a)<-[:R]-(b)` direction on the shared
//! handle; this suite proves the overlay and the pinned pass speak it too.
//! One staged in-edge (`9-[:R]->2`) must join the txn's incoming `RETURN b`
//! beside the durable source — paired, at the same instant, with the
//! shared handle still answering without it (no dirty read) — while the
//! incoming `RETURN a` stays `[2]` (a new source into the same destination
//! adds no new in-edge-holding vertex). Abort erases the staged source
//! from every surface, and `execute_gql_at` at the pre-stage frontier
//! agrees with the post-abort live answer — the overlay, the live scan,
//! and the pinned scan are one kernel in three moods, not three kernels.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const IN_RETURN_A: &str = "MATCH (a)<-[:R]-(b) RETURN a";
const IN_RETURN_B: &str = "MATCH (a)<-[:R]-(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-in-overlay-{}-{name}", std::process::id()))
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

/// The whole slice as one flow: stage, pair overlay against base, abort,
/// then agree with the pinned pass.
#[test]
fn the_incoming_overlay_sees_the_staged_source_and_abort_erases_it() {
    under_lab(0x10_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-in-edge");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let pre_stage = db.frontier().expect("healthy frontier");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(9), vec![], vec![]);
        batch.add_edge(EId(11), VId(9), VId(2), vec![]);
        txn.write(&mut db, batch)
            .expect("stages the new in-edge source");

        // THE PAIRING: the staged source joins the txn's incoming
        // projection, CGSE-sorted beside the durable one — while the shared
        // handle, at the same instant, answers without it.
        assert_eq!(
            txn.execute_gql(&db, IN_RETURN_B, &bind_r())
                .expect("the txn's incoming RETURN b executes"),
            vec![VId(1), VId(9)],
            "the staged source joins the overlay's incoming projection"
        );
        assert_eq!(
            db.execute_gql(IN_RETURN_B, &bind_r())
                .expect("the base incoming RETURN b executes"),
            vec![VId(1)],
            "DIRTY READ: the staged source leaked into the shared handle"
        );
        // The other projection is the control: a second source into the SAME
        // destination adds no new in-edge-holding vertex, so the overlay's
        // RETURN a must stay exactly [2] — a kernel that conflates the two
        // ends of the flipped edge widens this to [2, 9] and is caught.
        assert_eq!(
            txn.execute_gql(&db, IN_RETURN_A, &bind_r())
                .expect("the txn's incoming RETURN a executes"),
            vec![VId(2)],
            "the staged edge points at the same destination; a binds [2] still"
        );

        txn.abort();
        assert_eq!(
            db.execute_gql(IN_RETURN_B, &bind_r())
                .expect("the live incoming RETURN b executes after abort"),
            vec![VId(1)],
            "the aborted source is gone from the live answer"
        );
        // The pinned pass at the pre-stage frontier agrees with the
        // post-abort live answer: one kernel in three moods.
        assert_eq!(
            db.execute_gql_at(IN_RETURN_B, &bind_r(), pre_stage)
                .expect("the pinned incoming RETURN b executes"),
            vec![VId(1)],
            "as of the pre-stage frontier the staged source never existed"
        );
    });
}
