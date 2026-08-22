//! **The overlay MATCH is a relation-edge scan too**
//! (`fgdb-w4-g1-txn-core-qpmg.17`).
//!
//! `gql_exec_relation_scan.rs` pinned the kernel shape for the shared
//! handle; this suite pins the SAME shape for the transaction overlay —
//! plus the isolation pairing from the overlay suites. Three laws: a fully
//! staged edge is matched by its txn (paired with the base answering empty
//! at the same instant, and staying empty after abort across reopen); a
//! staged ISOLATE vertex adds no destination (the overlay must not treat
//! "staged" as "matched"); and two staged edges written destination-
//! descending come back CGSE-sorted from the overlay, then commit as ONE
//! sequence and answer identically from the durable stream.

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
    std::env::temp_dir().join(format!("fgdb-txn-edge-scan-{}-{name}", std::process::id()))
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

/// A staged edge — endpoints and all, nothing durable — is matched by its
/// txn, invisible to the shared handle at the same instant, and gone
/// everywhere after abort, including across a cold reopen.
#[test]
fn a_fully_staged_edge_is_matched_by_the_txn_only() {
    under_lab(0xe5_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-edge");
        {
            let mut db = Database::create(&commit, &dir, keys())
                .await
                .expect("creates");
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(9), vec![], vec![]);
            db.write(&commit, seed).await.expect("isolate seed commits");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            txn.write(&mut db, batch).expect("stages verts and edge");

            let overlay = txn
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(
                overlay,
                vec![VId(2)],
                "the staged edge answers through the overlay — and the \
                 durable isolate VId(9) still contributes nothing"
            );
            let base = db
                .execute_gql(PINNED, &bind_r())
                .expect("base MATCH executes");
            assert!(
                base.is_empty(),
                "DIRTY READ: the staged edge leaked into the shared handle: {base:?}"
            );
            txn.abort();

            assert!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes after abort")
                    .is_empty(),
                "the aborted edge is not in the shared answer"
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
            "the aborted edge left no durable trace for MATCH to find"
        );
    });
}

/// A staged ISOLATE vertex adds no destination: the overlay answer stays
/// exactly the durable one. "Staged by this txn" must not be conflated
/// with "matched by this statement" — the overlay kernel filters per
/// relation edge, same as the base kernel.
#[test]
fn a_staged_isolate_vertex_is_not_a_destination() {
    under_lab(0xe5_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-isolate");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(9), vec![], vec![]);
        txn.write(&mut db, batch).expect("stages the isolate");

        let overlay = txn
            .execute_gql(&db, PINNED, &bind_r())
            .expect("the txn's MATCH executes");
        assert_eq!(
            overlay,
            vec![VId(2)],
            "the staged isolate VId(9) is no destination of anything — the \
             overlay answer is exactly the durable one"
        );
        txn.abort();
    });
}

/// Two staged edges, destinations written DESCENDING: the overlay answer is
/// CGSE-sorted ascending, the txn commits as exactly one sequence, and the
/// durable stream answers identically across a cold reopen.
#[test]
fn staged_edges_come_back_sorted_and_commit_as_one_sequence() {
    under_lab(0xe5_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-sorted");
        {
            let mut db = Database::create(&commit, &dir, keys())
                .await
                .expect("creates");
            let before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.create_vertex(VId(3), vec![], vec![]);
            batch.create_vertex(VId(5), vec![], vec![]);
            // Descending destinations: 5 before 2. Ascending output must be
            // a sort, not an accident of staging order.
            batch.add_edge(EId(10), VId(1), VId(5), vec![]);
            batch.add_edge(EId(11), VId(3), VId(2), vec![]);
            txn.write(&mut db, batch).expect("stages both edges");

            let overlay = txn
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(
                overlay,
                vec![VId(2), VId(5)],
                "overlay destinations, CGSE-sorted ascending — not staging order"
            );

            let seq = txn
                .commit(&mut db, &commit)
                .await
                .expect("the staged txn commits");
            assert_eq!(seq.0, before.0 + 1, "one txn, one sequence");
            assert_eq!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes"),
                vec![VId(2), VId(5)],
                "after commit the shared answer equals the overlay's"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("executes after reopen"),
            vec![VId(2), VId(5)],
            "the durable stream answers identically"
        );
    });
}
