//! Transaction validation must survive arbitrary intervening ordinary writes.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteMismatchPolicy, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32]);
    let mut db = Database::open_memory(cx, keys).await.expect("database");
    let mut seed = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(8)] {
        seed.create_vertex(vid, vec![], vec![(P, CanonicalScalar::Int(0))]);
    }
    db.write(cx, seed).await.expect("seed");
    db
}

fn assert_write_conflict(result: Result<CommitSeq, WriteTxnError>) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-01",
            ..
        }))
    ));
}

async fn unrelated_writes(db: &mut Database<MemVfs>, cx: &CommitCx) {
    for value in 1..=3 {
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(8), P, Some(CanonicalScalar::Int(value)));
        db.write(cx, batch).await.expect("intervening ordinary write");
    }
}

#[test]
fn blind_write_cannot_lose_conflict_history_after_validator_resets() {
    let ((), report) = run_async_under_lab(0xfc10_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        txn.write(&mut db, staged).expect("stage without an explicit read");
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
        db.write(&cx, winner).await.expect("first writer");
        unrelated_writes(&mut db, &cx).await;
        let frontier = db.frontier().expect("frontier");
        assert_write_conflict(txn.commit(&mut db, &cx).await);
        assert_eq!(db.frontier().expect("no publication"), frontier);
        assert_eq!(
            db.vertex(VId(1)).expect("winner row").expect("vertex").props,
            vec![(P, CanonicalScalar::Int(10))]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn disjoint_staged_writes_survive_multiple_frontier_advances() {
    let ((), report) = run_async_under_lab(0xfc10_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        txn.write(&mut db, staged).expect("stage");
        unrelated_writes(&mut db, &cx).await;
        txn.commit(&mut db, &cx).await.expect("disjoint commit");
        assert_eq!(
            db.vertex(VId(1)).expect("result").expect("vertex").props,
            vec![(P, CanonicalScalar::Int(5))]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conditional_noop_retains_its_observed_target_dependency() {
    let ((), report) = run_async_under_lab(0xfc10_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.compare_and_set_vertex_property(
            VId(1), P, Some(CanonicalScalar::Int(99)), CanonicalScalar::Int(5),
            WriteMismatchPolicy::NoOp,
        );
        staged.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, staged).expect("mismatch is a lawful no-op");
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
        db.write(&cx, winner).await.expect("change the observed condition");
        unrelated_writes(&mut db, &cx).await;
        assert_write_conflict(txn.commit(&mut db, &cx).await);
        assert!(db.vertex(VId(99)).expect("no partial transaction").is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_vertex_delete_conflicts_with_new_incident_edges() {
    let ((), report) = run_async_under_lab(0xfc10_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.delete_vertex(VId(1));
        txn.write(&mut db, staged).expect("prepare an empty incident cascade");
        let mut winner = WriteBatch::new(R);
        winner.add_edge(EId(10), VId(2), VId(1), vec![]);
        db.write(&cx, winner).await.expect("insert an incoming incident edge");
        unrelated_writes(&mut db, &cx).await;
        let frontier = db.frontier().expect("frontier");
        assert_write_conflict(txn.commit(&mut db, &cx).await);
        assert_eq!(db.frontier().expect("no publication"), frontier);
        assert!(db.vertex(VId(1)).expect("endpoint remains live").is_some());
        assert!(db.edge(EId(10)).expect("winning edge remains live").is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
