//! **The labeled MATCH through the transaction overlay**
//! (`fgdb-w5-parsers-nje.5`, overlay slice).
//!
//! The label filter must hold across the durable/staged boundary in both
//! directions at once: a STAGED `:Person` source's destination joins the
//! txn's labeled answer (the overlay evaluates the label on staged
//! vertices, not only durable ones), while the shared handle at the same
//! instant answers without it — and the durable UNLABELED source's
//! destination stays out of both, so the overlay did not buy its staged
//! visibility by loosening the label.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const LABELED_B: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-label-overlay-{}-{name}", std::process::id()))
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
        .with_relation("R", R)
        .with_label("Person", PERSON)
}

/// Staged labeled source in, durable unlabeled source out, no dirty read.
#[test]
fn the_overlay_evaluates_the_label_on_staged_sources() {
    under_lab(0x1c_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-person");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(5), vec![PERSON], vec![]);
        staged.create_vertex(VId(6), vec![], vec![]);
        staged.add_edge(EId(12), VId(5), VId(6), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the labeled source");

        let overlay = txn
            .execute_gql(&db, LABELED_B, &bind_r_person())
            .expect("the txn's labeled MATCH executes");
        assert!(
            overlay.contains(&VId(2)) && overlay.contains(&VId(6)),
            "the durable AND the staged :Person sources both answer through \
             the overlay — the label is evaluated on staged vertices too: {overlay:?}"
        );
        assert!(
            !overlay.contains(&VId(4)),
            "the durable unlabeled source stays out of the overlay answer — \
             staged visibility was not bought by loosening the label: {overlay:?}"
        );

        let base = db
            .execute_gql(LABELED_B, &bind_r_person())
            .expect("the base labeled MATCH executes");
        assert!(
            base.contains(&VId(2)) && !base.contains(&VId(6)),
            "DIRTY READ: the staged source leaked into the shared handle: {base:?}"
        );
        assert!(
            !base.contains(&VId(4)),
            "the unlabeled source is out of the base answer too: {base:?}"
        );
        txn.abort();
    });
}
