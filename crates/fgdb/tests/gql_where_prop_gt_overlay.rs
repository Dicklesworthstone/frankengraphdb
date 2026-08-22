//! **`WHERE a.k > 1` through the transaction overlay**
//! (`fgdb-w5-parsers-nje.22`, overlay slice).
//!
//! The ordered comparator across the durable/staged boundary: the durable
//! source carries `k = 1` — ON the boundary, so the shared handle's
//! filtered answer is empty by strictness — and the staged `k = 9` source's
//! destination answers through the txn alone. The unfiltered pairing is
//! asserted first with BOTH faces non-empty, so the filtered emptiness is
//! attributably the comparator's (a broken overlay merge would already
//! fail the unfiltered pair), and the staged destination leaking into the
//! shared handle fails the dirty-read half of either pairing.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const GT_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";
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
    std::env::temp_dir().join(format!("fgdb-gt-overlay-{}-{name}", std::process::id()))
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

/// Staged greater-than carrier in the overlay answer, boundary carrier
/// keeping the base empty, no dirty read anywhere.
#[test]
fn the_overlay_sees_the_staged_greater_source_and_the_base_stays_empty() {
    under_lab(0x67_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-gt");
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

        // Attribution first: the unfiltered pairing, BOTH faces non-empty,
        // proves the overlay merge before the comparator enters.
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

        // The comparator pairing: strictness keeps the boundary carrier out
        // of both faces; the staged above-boundary carrier answers through
        // the txn alone.
        assert_eq!(
            txn.execute_gql(&db, GT_B, &bind_rk())
                .expect("the txn's WHERE a.k > 1 executes"),
            vec![VId(4)],
            "the staged k=9 source's destination answers through the overlay"
        );
        assert!(
            db.execute_gql(GT_B, &bind_rk())
                .expect("the base WHERE a.k > 1 executes")
                .is_empty(),
            "the durable k=1 sits ON the boundary: 1 > 1 is false, and the \
             staged row must not have leaked in either"
        );
        txn.abort();
    });
}
