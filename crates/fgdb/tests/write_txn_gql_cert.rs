//! Product witnesses for certified MATCH execution through a [`WriteTxn`]
//! overlay (`fgdb-w4-g1-txn-core-qpmg.8`).

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

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-txn-gql-cert-{}-{name}",
        std::process::id()
    ))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await
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

async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.expect("seed commits");
    db
}

fn staged_edge_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(8), vec![], vec![]);
    batch.create_vertex(VId(9), vec![], vec![]);
    batch.add_edge(EId(11), VId(8), VId(9), vec![]);
    batch
}

#[test]
fn certified_rows_are_overlay_rows_at_the_pinned_basis() {
    under_lab(0xce_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("overlay-basis");
        let mut db = seeded(&commit, &dir).await;

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        txn.write(&mut db, staged_edge_batch()).expect("stages edge");

        let overlay = txn
            .execute_gql(&db, PINNED, &bind_r())
            .expect("overlay MATCH executes");
        let (certified, certificate) = txn
            .execute_gql_certified(&db, PINNED, &bind_r())
            .expect("certified overlay MATCH executes");

        assert_eq!(certified, overlay);
        assert_eq!(certified, vec![VId(2), VId(9)]);
        assert_eq!(certificate.snapshot_seq, txn.basis());
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("shared MATCH executes"),
            vec![VId(2)],
            "the shared fold must not expose the staged destination"
        );
        txn.abort();
    });
}

#[test]
fn digest_is_stable_at_one_basis_and_changes_at_the_next() {
    under_lab(0xce_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("seq-sensitive");
        let mut db = seeded(&commit, &dir).await;

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        txn.write(&mut db, staged_edge_batch()).expect("stages edge");
        let (first_rows, first) = txn
            .execute_gql_certified(&db, PINNED, &bind_r())
            .expect("first certified MATCH executes");
        let (second_rows, second) = txn
            .execute_gql_certified(&db, PINNED, &bind_r())
            .expect("second certified MATCH executes");
        assert_eq!(first_rows, second_rows);
        assert_eq!(first.digest, second.digest);
        assert_eq!(first.snapshot_seq, second.snapshot_seq);

        txn.commit(&mut db, &commit)
            .await
            .expect("staged edge commits");
        let next = db.begin(&txn_cx).expect("next-basis txn begins");
        let (_, after) = next
            .execute_gql_certified(&db, PINNED, &bind_r())
            .expect("new-basis certified MATCH executes");

        assert_ne!(first.snapshot_seq, after.snapshot_seq);
        assert_ne!(first.digest, after.digest);
        next.abort();
    });
}
