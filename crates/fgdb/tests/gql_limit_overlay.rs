use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const LIMITED: &str = "MATCH (a)-[:R]->(b) RETURN b LIMIT 1";
const UNLIMITED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn limit_overlay_keeps_staged_smallest_destination_without_dirty_read() {
    let ((), report) = run_async_under_lab(0x45_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-limit-overlay-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        for vid in [VId(1), VId(3), VId(4), VId(6)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(4), vec![]);
        seed.add_edge(EId(11), VId(3), VId(6), vec![]);
        db.write(&commit, seed).await.expect("durable edges commit");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(2), vec![], vec![]);
        staged.add_edge(EId(12), VId(1), VId(2), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the smallest destination");

        let bind = RelationBind::new().with_relation("R", R);
        assert_eq!(
            txn.execute_gql(&db, LIMITED, &bind)
                .expect("limited overlay MATCH executes"),
            vec![VId(2)]
        );
        assert_eq!(
            txn.execute_gql(&db, UNLIMITED, &bind)
                .expect("unlimited overlay MATCH executes"),
            vec![VId(2), VId(4), VId(6)]
        );
        assert_eq!(
            db.execute_gql(LIMITED, &bind)
                .expect("limited base MATCH executes"),
            vec![VId(4)],
            "the shared handle must not see staged destination 2"
        );
        assert_eq!(
            db.execute_gql(UNLIMITED, &bind)
                .expect("unlimited base MATCH executes"),
            vec![VId(4), VId(6)]
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
