//! Ensure-by-triple observes actual edge identities, not only the requested ID.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

#[test]
fn existing_ensure_alias_survives_only_when_its_observed_edge_survives() {
    let ((), report) = run_async_under_lab(0xfc10_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        for delete_existing in [false, true] {
            let keys = DatabaseKeys::new(
                [0x71; 32],
                DatabaseSecurityNamespaceId([0x72; 32]),
                [0x73; 32],
            );
            let mut db = Database::open_memory(&cx, keys).await.expect("database");
            let mut seed = WriteBatch::new(RelationId(1));
            for id in [1, 2, 8] {
                seed.create_vertex(VId(id), vec![], vec![]);
            }
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(&cx, seed).await.expect("seed the existing triple");
            let mut txn = db.begin(&txn_cx).expect("begin");
            let mut stage = WriteBatch::new(RelationId(1));
            stage.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage)
                .expect("ensure resolves through EId(10)");
            if delete_existing {
                let mut winner = WriteBatch::new(RelationId(1));
                winner.delete_edge(EId(10));
                db.write(&cx, winner)
                    .await
                    .expect("delete the actual alias");
            }
            for value in 1..=2 {
                let mut unrelated = WriteBatch::new(RelationId(1));
                unrelated.set_vertex_property(
                    VId(8),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(value)),
                );
                db.write(&cx, unrelated)
                    .await
                    .expect("advance/reset without touching the triple");
            }
            let frontier = db.frontier().expect("frontier");
            let result = txn.commit(&mut db, &cx).await;
            if delete_existing {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().expect("no partial publication"), frontier);
                assert!(db.vertex(VId(99)).expect("staged output absent").is_none());
                assert!(
                    db.edge(EId(10))
                        .expect("winner's deletion retained")
                        .is_none()
                );
            } else {
                result.expect("unrelated changes must not globally fence the transaction");
                assert!(
                    db.vertex(VId(99))
                        .expect("staged output published")
                        .is_some()
                );
                assert!(db.edge(EId(10)).expect("actual edge retained").is_some());
            }
            assert!(
                db.edge(EId(999))
                    .expect("ensure's unused alias remains absent")
                    .is_none()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
