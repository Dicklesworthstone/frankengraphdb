//! **The overlay MATCH hides what the txn deleted**
//! (`fgdb-w4-g1-txn-core-qpmg.18`).
//!
//! The additive overlay suites proved staged CREATES appear through the
//! txn's MATCH; this one proves the subtractive half, which a delta kernel
//! gets wrong for free: an overlay that UNIONS staged edges over the
//! durable scan can never make a durable row disappear. A staged
//! `delete_edge` must empty the txn's MATCH while the shared handle — at
//! the same instant — still serves the durable answer; a staged
//! `delete_vertex` must do the same through its cascade. Both directions
//! of the pairing are load-bearing: overlay-empty alone is satisfiable by
//! a broken scan, base-still-full alone by a dirty delete.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-txn-del-hide-{}-{name}", std::process::id()))
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

/// One durable `:R` edge `VId(1) -> VId(2)` under the explicit `EId(1)`
/// the delete below names.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.add_edge(EId(1), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// A staged `delete_edge` empties the txn's MATCH while the shared handle
/// still serves the durable destination at the same instant; abort throws
/// the delete away, and reopen still answers `[VId(2)]`.
#[test]
fn a_staged_edge_delete_hides_the_destination_from_the_txn_only() {
    under_lab(0xdd_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("edge-delete");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(R);
            batch.delete_edge(EId(1));
            txn.write(&mut db, batch).expect("stages the edge delete");

            let overlay = txn
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert!(
                overlay.is_empty(),
                "a union-only overlay cannot hide the deleted edge: {overlay:?}"
            );
            assert_eq!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes"),
                vec![VId(2)],
                "DIRTY DELETE: the staged retirement leaked into the shared \
                 handle before commit"
            );
            txn.abort();

            assert_eq!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes after abort"),
                vec![VId(2)],
                "the aborted delete changed nothing"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("executes after reopen"),
            vec![VId(2)],
            "the durable edge survived the aborted delete"
        );
    });
}

/// A staged `delete_vertex` of the DESTINATION hides it through the
/// cascade: the overlay MATCH is empty (the incident edge dies with the
/// vertex), the shared handle keeps answering until commit, the commit is
/// one sequence, and reopen agrees the destination is gone.
#[test]
fn a_staged_vertex_delete_cascades_out_of_the_overlay_match() {
    under_lab(0xdd_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("vertex-delete");
        {
            let mut db = seeded(&commit, &dir).await;
            let before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(R);
            batch.delete_vertex(VId(2));
            txn.write(&mut db, batch).expect("stages the vertex delete");

            let overlay = txn
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert!(
                overlay.is_empty(),
                "the cascade retires the incident edge; a dead vertex is no \
                 destination: {overlay:?}"
            );
            assert_eq!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes"),
                vec![VId(2)],
                "the shared handle serves the durable answer until commit"
            );

            let seq = txn
                .commit(&mut db, &commit)
                .await
                .expect("the delete txn commits");
            assert_eq!(seq.0, before.0 + 1, "one txn, one sequence");
            assert!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes after commit")
                    .is_empty(),
                "after commit the shared answer agrees with the overlay's"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert!(
            db.execute_gql(PINNED, &bind_r())
                .expect("executes after reopen")
                .is_empty(),
            "the cascade is durable: the destination is gone from the answer"
        );
    });
}
