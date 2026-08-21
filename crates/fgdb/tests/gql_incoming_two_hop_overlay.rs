use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const IN_TWO_HOP_C: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn bind() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

#[test]
fn incoming_two_hop_overlay_sees_a_staged_composed_source() {
    let ((), report) = run_async_under_lab(0x37_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-incoming-two-hop-overlay-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4, 7, 8, 9] {
            r_batch.create_vertex(VId(vid), vec![], vec![]);
        }
        r_batch.add_edge(EId(10), VId(2), VId(1), vec![]);
        r_batch.add_edge(EId(11), VId(7), VId(1), vec![]);
        db.write(&commit, r_batch).await.expect("R edges commit");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(4), VId(2), vec![]);
        s_batch.add_edge(EId(21), VId(8), VId(9), vec![]);
        db.write(&commit, s_batch).await.expect("S edges commit");

        let bind = bind();
        let before = db
            .execute_gql(IN_TWO_HOP_C, &bind)
            .expect("durable incoming two-hop MATCH");
        assert_eq!(before, vec![VId(4)]);

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(S);
        staged.create_vertex(VId(5), vec![], vec![]);
        staged.add_edge(EId(22), VId(5), VId(2), vec![]);
        txn.write(&mut db, staged)
            .expect("composed S source stages");

        assert_eq!(
            txn.execute_gql(&db, IN_TWO_HOP_C, &bind)
                .expect("overlay incoming two-hop MATCH"),
            vec![VId(4), VId(5)]
        );
        assert_eq!(
            db.execute_gql(IN_TWO_HOP_C, &bind)
                .expect("shared incoming two-hop MATCH"),
            before,
            "the staged source must not leak into the shared fold"
        );
        assert!(
            txn.execute_gql(&db, OUT_TWO_HOP_C, &bind)
                .expect("overlay outgoing direction control")
                .is_empty(),
            "the reversed fixture has no outgoing two-hop path"
        );

        txn.abort();
        assert_eq!(
            db.execute_gql(IN_TWO_HOP_C, &bind)
                .expect("post-abort incoming two-hop MATCH"),
            before
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
