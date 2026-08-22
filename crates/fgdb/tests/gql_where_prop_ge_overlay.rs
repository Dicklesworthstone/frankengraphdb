//! **`WHERE a.k >= 1` through the transaction overlay**
//! (`fgdb-w5-parsers-nje.26`, overlay slice).
//!
//! The non-strict comparator across the durable/staged boundary — and its
//! signature difference from the strict overlay suite: the durable source
//! sits ON the boundary, so here it ANSWERS on both faces (`1 >= 1`),
//! while the staged `k = 9` source's destination joins the txn's answer
//! alone. Both filtered faces are non-empty, so the pairing is a pure
//! dirty-read check; the unfiltered pairing is asserted first for
//! attribution as always.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const GE_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b";
const PLAIN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-ge-overlay-{}-{name}", std::process::id()))
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

fn bind_rk() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("k", K)
}

/// The boundary carrier answers on both faces, the staged carrier on the
/// overlay alone.
#[test]
fn the_boundary_answers_both_faces_and_the_staged_source_the_overlay_alone() {
    under_lab(0x6e_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-ge");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
        staged.create_vertex(VId(4), vec![], vec![]);
        staged.add_edge(EId(11), VId(3), VId(4), vec![]);
        txn.write(&mut db, staged).expect("stages the k=9 source");

        // Attribution first: the unfiltered pairing proves the overlay merge
        // before the comparator enters.
        assert_eq!(
            txn.execute_gql(&db, PLAIN_B, &bind_rk())
                .expect("the txn's unfiltered MATCH executes"),
            vec![VId(2), VId(4)],
            "the staged destination joins the overlay rows"
        );
        assert_eq!(
            db.execute_gql(PLAIN_B, &bind_rk())
                .expect("the base unfiltered MATCH executes"),
            vec![VId(2)],
            "DIRTY READ: the staged destination leaked into the shared handle"
        );

        // The non-strict pairing: 1 >= 1 is TRUE, so the durable boundary
        // carrier answers on BOTH faces — the difference between the faces
        // is exactly the staged row, making this a pure dirty-read check
        // (the strict overlay suite's base was empty; this one must not be).
        assert_eq!(
            txn.execute_gql(&db, GE_B, &bind_rk())
                .expect("the txn's WHERE a.k >= 1 executes"),
            vec![VId(2), VId(4)],
            "boundary AND staged carriers answer through the overlay"
        );
        assert_eq!(
            db.execute_gql(GE_B, &bind_rk())
                .expect("the base WHERE a.k >= 1 executes"),
            vec![VId(2)],
            "the boundary carrier answers on the shared handle too — and \
             ONLY it: the staged row must not have leaked"
        );
        txn.abort();
    });
}
