//! **SKIP through the transaction overlay**
//! (`fgdb-w5-parsers-nje.13`, overlay slice).
//!
//! SKIP drops the FRONT of the CGSE-sorted row set, so which row it drops
//! depends on which rows exist — and a staged destination SMALLER than
//! every durable one moves the cut line: the txn's `SKIP 1` drops the
//! staged 2 and answers the durable pair, while the shared handle — which
//! must not see the staged row — drops its own smallest durable 4 and
//! answers `[6]`. The two `SKIP 1` answers OVERLAP on nothing but 6 and
//! differ on both other rows, so a dirty read or a skip applied before
//! the overlay merge each fail loudly; the unskipped statements are
//! asserted beside them so the difference is provably SKIP's.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const SKIP1: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 1";
const UNSKIPPED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-skip-overlay-{}-{name}", std::process::id()))
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

/// The staged smallest destination moves the txn's cut line and only the
/// txn's.
#[test]
fn skip_drops_the_staged_smallest_dest_in_the_overlay_only() {
    under_lab(0x5c_a1, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("cut-line");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        for vid in [1u128, 3, 4, 6] {
            seed.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(4), vec![]);
        seed.add_edge(EId(11), VId(3), VId(6), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(2), vec![], vec![]);
        staged.add_edge(EId(12), VId(1), VId(2), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the smallest destination");

        // The unskipped pair first: the overlay merge itself, so the SKIP
        // differences below are attributable to SKIP alone.
        assert_eq!(
            txn.execute_gql(&db, UNSKIPPED, &bind_r())
                .expect("the txn's unskipped MATCH executes"),
            vec![VId(2), VId(4), VId(6)],
            "the staged destination joins the overlay rows"
        );
        assert_eq!(
            db.execute_gql(UNSKIPPED, &bind_r())
                .expect("the base unskipped MATCH executes"),
            vec![VId(4), VId(6)],
            "DIRTY READ: the staged destination leaked into the shared handle"
        );

        // SKIP 1 on each face drops each face's OWN smallest row: the
        // staged 2 in the overlay, the durable 4 on the shared handle. The
        // answers differ on every row but 6 — a skip applied before the
        // overlay merge (dropping 4 from the txn too) or a dirty read
        // (dropping 2 from the base too) each fail.
        assert_eq!(
            txn.execute_gql(&db, SKIP1, &bind_r())
                .expect("the txn's SKIP 1 executes"),
            vec![VId(4), VId(6)],
            "the overlay's front row is the staged 2, and SKIP drops it"
        );
        assert_eq!(
            db.execute_gql(SKIP1, &bind_r())
                .expect("the base SKIP 1 executes"),
            vec![VId(6)],
            "the shared handle's front row is the durable 4 — the staged 2 \
             must not have shifted this cut line"
        );
        txn.abort();
    });
}
