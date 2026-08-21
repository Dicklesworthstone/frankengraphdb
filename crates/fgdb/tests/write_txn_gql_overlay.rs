//! **The pinned MATCH through the transaction overlay**
//! (`fgdb-w4-g1-txn-core-qpmg.4`).
//!
//! `WriteTxn::vertex` proved read-your-writes for point reads; this suite
//! proves it for the language surface: `MATCH (a)-[:R]->(b) RETURN b`
//! executed THROUGH the transaction sees the txn's staged edge, while the
//! same statement on the shared `Database` handle — at the same instant —
//! does not. The pairing is the law: overlay-visible alone is satisfiable
//! by an engine that leaked the staged write into the shared fold (dirty
//! read), and base-invisible alone is satisfiable by an overlay that sees
//! nothing.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (this wave's landed name):
//! - `WriteTxn::execute_gql(&self, &Database<V>, src, &RelationBind)` → the
//!   same `Result<Vec<VId>, GqlError>` shape `Database::execute_gql` has,
//!   answered from pinned basis + this txn's staged batches, CGSE-sorted.
//! Until it lands this file fails to compile — deliberately; do not weaken
//! it to make it compile.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
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
    std::env::temp_dir().join(format!("fgdb-txn-gql-{}-{name}", std::process::id()))
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

/// One committed `:R` edge `VId(1) -> VId(2)`, so the base MATCH already has
/// a non-empty answer: the staged destination below must APPEAR BESIDE a
/// real one, not be the only row an empty-graph counterfeit could fake.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Stage `VId(8) -[:R]-> VId(9)` inside `txn`: two creates and the edge.
fn staged_edge_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(8), vec![], vec![]);
    batch.create_vertex(VId(9), vec![], vec![]);
    batch.add_edge(EId(11), VId(8), VId(9), vec![]);
    batch
}

/// THE PAIRING: the txn's MATCH sees the staged destination (sorted beside
/// the committed one), the shared handle's MATCH — at the same instant —
/// does not.
#[test]
fn staged_edge_is_matched_by_the_txn_and_not_by_the_base() {
    under_lab(0xe0_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("pairing");
        let mut db = seeded(&commit, &dir).await;

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        txn.write(&mut db, staged_edge_batch()).expect("stages the edge");

        let overlay = txn
            .execute_gql(&db, PINNED, &bind_r())
            .expect("the txn's MATCH executes");
        assert_eq!(
            overlay,
            vec![VId(2), VId(9)],
            "the overlay MATCH sees the committed destination AND the staged \
             one, CGSE-sorted"
        );
        let base = db.execute_gql(PINNED, &bind_r()).expect("the base MATCH executes");
        assert_eq!(
            base,
            vec![VId(2)],
            "DIRTY READ: the staged edge leaked into the shared handle's MATCH"
        );
        txn.abort();
    });
}

/// Abort: the staged destination never reaches the shared MATCH — live and
/// across a cold reopen. The visibility control first, so the absence below
/// cannot be vacuously green.
#[test]
fn aborted_staged_edge_never_reaches_the_base_match() {
    under_lab(0xe0_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            txn.write(&mut db, staged_edge_batch()).expect("stages the edge");
            assert!(
                txn.execute_gql(&db, PINNED, &bind_r())
                    .expect("the txn's MATCH executes")
                    .contains(&VId(9)),
                "control: the staged destination was visible to its txn"
            );
            txn.abort();

            assert_eq!(
                db.execute_gql(PINNED, &bind_r()).expect("base MATCH executes"),
                vec![VId(2)],
                "the aborted edge is not in the shared answer"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r()).expect("executes after reopen"),
            vec![VId(2)],
            "the aborted edge left no durable trace for MATCH to find"
        );
    });
}

/// Commit: the staged destination joins the shared MATCH at exactly one new
/// sequence, and survives a cold reopen.
#[test]
fn committed_staged_edge_joins_the_base_match_at_one_sequence() {
    under_lab(0xe0_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit");
        {
            let mut db = seeded(&commit, &dir).await;
            let before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            txn.write(&mut db, staged_edge_batch()).expect("stages the edge");
            let seq = txn
                .commit(&mut db, &commit)
                .await
                .expect("the staged txn commits");
            assert_eq!(
                seq.0,
                before.0 + 1,
                "one txn, one sequence — however the overlay staged it"
            );

            assert_eq!(
                db.execute_gql(PINNED, &bind_r()).expect("base MATCH executes"),
                vec![VId(2), VId(9)],
                "after commit the shared MATCH serves the new destination"
            );
        }

        let db = Database::open(&commit, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r()).expect("executes after reopen"),
            vec![VId(2), VId(9)],
            "the committed edge is durable for the language surface too"
        );
    });
}
