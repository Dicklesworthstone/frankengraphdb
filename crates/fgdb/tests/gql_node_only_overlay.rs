//! **The node-only MATCH through the transaction overlay**
//! (`fgdb-w5-parsers-nje.7`, overlay slice).
//!
//! The edgeless labeled scan must see staged VERTICES: a staged `:Person`
//! isolate — no edge anywhere near it — joins the txn's answer beside the
//! durable labeled rows, which is precisely what an overlay built on the
//! staged EDGE table can never produce. Paired at the same instant, the
//! shared handle answers without it, and the unlabeled vertices stay out
//! of both answers, so the staged visibility was not bought by loosening
//! the label or the scan.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const NODE_ONLY: &str = "MATCH (a:Person) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-only-ov-{}-{name}", std::process::id()))
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

fn bind_r_person() -> RelationBind {
    RelationBind::new()
        .with_label("Person", PERSON)
        .with_relation("R", R)
}

/// Staged labeled isolate in the overlay answer, absent from the base,
/// unlabeled vertices in neither.
#[test]
fn the_overlay_scan_sees_the_staged_person_isolate() {
    under_lab(0x41_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-isolate");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![PERSON], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(5), vec![PERSON], vec![]);
        txn.write(&mut db, staged)
            .expect("stages the labeled isolate");

        let overlay = txn
            .execute_gql(&db, NODE_ONLY, &bind_r_person())
            .expect("the txn's node-only MATCH executes");
        assert!(
            overlay.contains(&VId(1)) && overlay.contains(&VId(3)) && overlay.contains(&VId(5)),
            "the durable isolate, the durable source, AND the staged \
             isolate all answer through the overlay — a staged-edge-table \
             overlay can never produce the edgeless 5: {overlay:?}"
        );
        assert!(
            !overlay.contains(&VId(2)) && !overlay.contains(&VId(4)),
            "the unlabeled vertices stay out of the overlay answer: {overlay:?}"
        );

        let base = db
            .execute_gql(NODE_ONLY, &bind_r_person())
            .expect("the base node-only MATCH executes");
        assert!(
            base.contains(&VId(1)) && base.contains(&VId(3)) && !base.contains(&VId(5)),
            "DIRTY READ: the staged isolate leaked into the shared handle: {base:?}"
        );
        assert!(
            !base.contains(&VId(2)) && !base.contains(&VId(4)),
            "the unlabeled vertices stay out of the base answer too: {base:?}"
        );
        txn.abort();
    });
}
